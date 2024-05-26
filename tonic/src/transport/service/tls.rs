use std::{fmt, sync::Arc};

use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::{
    rustls::{pki_types::ServerName, ClientConfig, RootCertStore, ServerConfig},
    TlsAcceptor as RustlsAcceptor, TlsConnector as RustlsConnector,
};

use self::rustls_keys::{add_certs_from_pem, load_identity};
use super::io::BoxedIo;
use crate::transport::{
    server::{Connected, TlsStream},
    Certificate, Identity,
};
use hyper_util::rt::TokioIo;

/// h2 alpn in plain format for rustls.
const ALPN_H2: &[u8] = b"h2";

#[derive(Debug)]
enum TlsError {
    H2NotNegotiated,
    CertificateParseError,
    PrivateKeyParseError,
}

#[derive(Clone)]
pub(crate) struct TlsConnector {
    config: Arc<ClientConfig>,
    domain: Arc<ServerName<'static>>,
    assume_http2: bool,
}

impl TlsConnector {
    pub(crate) fn new(
        ca_cert: Option<Certificate>,
        identity: Option<Identity>,
        domain: &str,
        assume_http2: bool,
    ) -> Result<Self, crate::Error> {
        let builder = ClientConfig::builder();
        let mut roots = RootCertStore::empty();

        #[cfg(feature = "tls-roots")]
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs()?.into_iter());

        #[cfg(feature = "tls-webpki-roots")]
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        if let Some(cert) = ca_cert {
            add_certs_from_pem(std::io::Cursor::new(cert.as_ref()), &mut roots)?;
        }

        let builder = builder.with_root_certificates(roots);
        let mut config = match identity {
            Some(identity) => {
                let (client_cert, client_key) = load_identity(identity)?;
                builder.with_client_auth_cert(client_cert, client_key)?
            }
            None => builder.with_no_client_auth(),
        };

        let domain: ServerName<'static> = ServerName::try_from(domain)?.to_owned();

        config.alpn_protocols.push(ALPN_H2.into());
        Ok(Self {
            config: Arc::new(config),
            domain: Arc::new(domain),
            assume_http2,
        })
    }

    pub(crate) async fn connect<I>(&self, io: I) -> Result<BoxedIo, crate::Error>
    where
        I: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let io = RustlsConnector::from(self.config.clone())
            .connect(self.domain.as_ref().to_owned(), io)
            .await?;

        // Generally we require ALPN to be negotiated, but if the user has
        // explicitly set `assume_http2` to true, we'll allow it to be missing.
        let assume_http2 = self.assume_http2;

        Ok({
            let (_, session) = io.get_ref();

            match session.alpn_protocol() {
                Some(b) if b == b"h2" => (),
                _ if assume_http2 => (),
                _ => return Err(TlsError::H2NotNegotiated.into()),
            };

            BoxedIo::new(TokioIo::new(io))
        })
    }
}

impl fmt::Debug for TlsConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsConnector").finish()
    }
}

#[derive(Clone)]
pub(crate) struct TlsAcceptor {
    inner: Arc<ServerConfig>,
}

impl TlsAcceptor {
    pub(crate) fn new(
        identity: Identity,
        client_ca_root: Option<Certificate>,
        client_auth_optional: bool,
    ) -> Result<Self, crate::Error> {
        let builder = ServerConfig::builder();

        let builder = match (client_ca_root, client_auth_optional) {
            (None, _) => builder.with_no_client_auth(),
            (Some(cert), true) => {
                use tokio_rustls::rustls::server::WebPkiClientVerifier;
                let mut roots = RootCertStore::empty();
                rustls_keys::add_certs_from_pem(std::io::Cursor::new(cert.as_ref()), &mut roots)?;
                builder.with_client_cert_verifier(
                    WebPkiClientVerifier::builder(Arc::new(roots))
                        .allow_unauthenticated()
                        .build()?
                        .into(),
                )
            }
            (Some(cert), false) => {
                use tokio_rustls::rustls::server::WebPkiClientVerifier;
                let mut roots = RootCertStore::empty();
                rustls_keys::add_certs_from_pem(std::io::Cursor::new(cert.as_ref()), &mut roots)?;
                builder.with_client_cert_verifier(
                    WebPkiClientVerifier::builder(Arc::new(roots))
                        .build()?
                        .into(),
                )
            }
        };

        let (cert, key) = load_identity(identity)?;
        let mut config = builder.with_single_cert(cert, key)?;

        config.alpn_protocols.push(ALPN_H2.into());
        Ok(Self {
            inner: Arc::new(config),
        })
    }

    pub(crate) async fn accept<IO>(&self, io: IO) -> Result<TlsStream<IO>, crate::Error>
    where
        IO: AsyncRead + AsyncWrite + Connected + Unpin + Send + 'static,
    {
        let acceptor = RustlsAcceptor::from(self.inner.clone());
        acceptor.accept(io).await.map_err(Into::into)
    }
}

impl fmt::Debug for TlsAcceptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsAcceptor").finish()
    }
}

impl fmt::Display for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TlsError::H2NotNegotiated => write!(f, "HTTP/2 was not negotiated."),
            TlsError::CertificateParseError => write!(f, "Error parsing TLS certificate."),
            TlsError::PrivateKeyParseError => write!(
                f,
                "Error parsing TLS private key - no RSA or PKCS8-encoded keys found."
            ),
        }
    }
}

impl std::error::Error for TlsError {}

mod rustls_keys {
    use std::io::{self, Cursor};

    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tokio_rustls::rustls::RootCertStore;

    use crate::transport::service::tls::TlsError;
    use crate::transport::Identity;

    pub(super) fn load_rustls_private_key<'a>(
        mut cursor: std::io::Cursor<&'a [u8]>,
    ) -> Result<PrivateKeyDer<'a>, crate::Error> {
        while let Ok(Some(item)) = rustls_pemfile::read_one(&mut cursor) {
            match item {
                rustls_pemfile::Item::Pkcs1Key(key) => return Ok(PrivateKeyDer::Pkcs1(key)),
                rustls_pemfile::Item::Pkcs8Key(key) => return Ok(PrivateKeyDer::Pkcs8(key)),
                rustls_pemfile::Item::Sec1Key(key) => return Ok(PrivateKeyDer::Sec1(key)),
                _ => continue,
            }
        }

        // Otherwise we have a Private Key parsing problem
        Err(Box::new(TlsError::PrivateKeyParseError))
    }

    pub(crate) fn load_identity(
        identity: Identity,
    ) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), crate::Error> {
        let cert: Vec<CertificateDer<'static>> = {
            let mut cert = std::io::Cursor::new(identity.cert.as_ref());
            rustls_pemfile::certs(&mut cert)
                .map(|cr| cr.map(|c| c.to_owned()))
                .collect::<Result<_, io::Error>>()?
        };

        let key = {
            let key = std::io::Cursor::new(identity.key.as_ref());
            match load_rustls_private_key(key) {
                Ok(key) => key.clone_key(),
                Err(e) => {
                    return Err(e);
                }
            }
        };

        Ok((cert, key))
    }

    pub(crate) fn add_certs_from_pem(
        mut certs: Cursor<&[u8]>,
        roots: &mut RootCertStore,
    ) -> Result<(), crate::Error> {
        let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut certs)
            .map(|cr| cr.map(|c| c.to_owned()))
            .collect::<Result<_, io::Error>>()?;

        let (_, ignored) = roots.add_parsable_certificates(certs.into_iter());
        match ignored == 0 {
            true => Ok(()),
            false => Err(Box::new(TlsError::CertificateParseError)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    // generated by: openssl ecparam -keygen -name 'prime256v1'
    const SIMPLE_EC_KEY: &str = r#"-----BEGIN EC PARAMETERS-----
BggqhkjOPQMBBw==
-----END EC PARAMETERS-----
-----BEGIN EC PRIVATE KEY-----
MHcCAQEEICIDyh40kMVWGDAYr1gXnMfeMeO3zXYigOaWrg5SNB+zoAoGCCqGSM49
AwEHoUQDQgAEacJyVg299dkPTzUaMbOmACUfF67yp+ZrDhXVjn/5WxBAgjcmFBHg
Tw8dfwpMzaJPXX5lWYzP276fcmbRO25CXw==
-----END EC PRIVATE KEY-----"#;

    // generated by: openssl genpkey -algorithm rsa
    const SIMPLE_PKCS8_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIICdwIBADANBgkqhkiG9w0BAQEFAASCAmEwggJdAgEAAoGBAKHkX1YIvqOIAllD
5fKcIxu2kYjIxxAAQrOBRTloGZUKdPFQY1RANB4t/LEaI5/NJ6NK4915pTn35QAQ
zHJl+X4rNFMgVt+o/nY40PgrQxyyv5A0/URp+iS8Yn3GKt3q6p4zguiO9uNXhiiD
b+VKIFRDm4bHR2yM7pNJ0kMdoattAgMBAAECgYAMpw6UaMaNfVnBpD7agT11MwWY
zShRpdOQt++zFuG49kJBgejhcssf+LQhG0vhd2U7q+S3MISrTSaGpMl1v2aKR/nV
G7X4Bb6X8vrVSMrfze2loT0aNri9jKDZkD/muy6+9JkhRa03NOdhDdseokmcqF3L
xsU4BUOOFYb23ycoeQJBANOGxbZu/3BqsPJMQmXWo1CXuaviZ83lTczPtrz9mJVl
Zs/KmGnJ8I2Azu/dlYXsHRvbIbqA93l1M3GnsWl5IxsCQQDD7hKvOY6qzUNyj+R4
vul/3xaqjiTj59f3jN7Fh6+9AY+WfvEkWfyUUAXY74z43wBgtORfMXnZnjFO96tJ
sswXAkBDYDtb19E/cox4MTg5DfwpMJrwmAYufCqi4Uq4uiI++/SanVKc57jaqbvA
hZkZ9lJzTAJbULcDFgTT3/FPwkkfAkEAqbSDMIzdGuox2n/x9/f8jcpweogmQdUl
xgCZUGSnfkFk2ojXW5Ip6Viqx+0toL6fOCRWjnFvRmPz958kGPCqPwJBAID4y7XV
peOO6Yadu0YbSmFNluRebia6410p5jR21LhG1ty2h22xVhlBWjOC+TyDuKwhmiYT
ed50S3LR1PWt4zE=
-----END PRIVATE KEY-----"#;

    // generated by: openssl genrsa
    const SIMPLE_RSA_KEY: &str = r#"-----BEGIN RSA PRIVATE KEY-----
MIIEogIBAAKCAQEAoEILGds1/RGBHT7jM4R+EL24sQ6Bsn14GgTHc7WoZ7lainEH
H/n+DtHCYUXYyJnN5AMIi3pkigCP1hdXXBQga3zs3lXoi/mAMkT6vjuqQ7Xg5/95
ABx5Ztyy25mZNaXm77glyAzSscKHxWYooXVJYG4C3SGuBJJ1zVjxen6Rkzse5Lpr
yZOUUeqeV3M6KbJ/dkR37HFQVwmlctQukFnb4kozFBQDDnkXi9jT/PH00g6JpW3z
YMzdMq2RMadJ0dzYv62OtdtqmQpVz0dRu/yODV4DkhrWwgPRj2uY4DnYthzILESB
x41gxHj+jqo6NW+C+0fr6uh2CXtD0p+ZVANtBQIDAQABAoIBAE7IaOCrLV1dr5WL
BvKancbpHdSdBxGSMOrJkqvFkCZ9ro8EhbYolcb/Q4nCZpInWlpPS3IWFzroj811
6BJyKoXtAh1DKnE1lNohowrGFiv3S7uBkiCF3wC8Wokud20yQ9dxNdGkzCdrNIfM
cwj8ubfYHTxMhFnnDlaG9R98/V/dFy0FLxL37eMP/heMbcwKKm9P/G2FqvuCn8a4
FoPbAfvaR64IGCybjoiTjUD7xMHIV4Gr5K07br2TzG2zVlFTacoqXyGBbVVy+ibt
QMh0sn+rMkAy+cFse+yCYZeAFa4FzwGz43sdFviU7uvLG7yXpvZ+uDACFzxlxUVg
v57r1cECgYEA1MMJEe6IunDUyuzRaFNTfQX16QcAv/xLN/1TtVB3HUX5p2bIZKDr
XEl0NCVOrCoz5RsYqbtGmp8B4Yxl3DeX+WeWeD9/f2ZTVGWyBx1N6dZ5hRsyfzG/
xVBUqYxkChjXQ20cNtf8u7JKdnVjOJen9M92nXhFRTwgH83Id4gPp70CgYEAwNN8
lvVJnd05ekyf1qIKOSyKiSGnGa5288PpqsjYMZisXy12y4n8cK2pX5Z5PICHeJVu
K99WdTtO7Q4ghCXRB1jR5pTd4/3/3089SQyDnWz9jlA3pGWcSLDTB1dBJXpMQ6yG
cR2dX5hPDNIdKsc+9Bl/OF5PScvGVUYv4SLF6ukCgYAVhh2WyNDgO6XrWYXdzgA2
N7Im/uReh8F8So57W0aRmZCmFMnVFEp7LZsp41RQKnzRgqo+EYoU/l0MWk27t4wS
WR5pz9KwKsPnV9poydgl/eKRSq0THQ9PgM7v0BoWw2iTk6g1DCivPFw4G6wL/5uo
MozHZXFsjaaaUREktokO6QKBgC3Dg7RILtqaoIOYH+9OseJz4cU+CWyc7XpZKuHv
nO/YbkCAh8syyojrjmEzUz66umwx+t3KubhFBSxZx/nVB9EYkWiKOEdeBxY2tjLa
F3qLXXojK7GGtBrEbLE3UizU47jD/3xlLO59NXWzgFygwR4p1vnH2EWJaV7fs4lZ
OWPRAoGAL0nX0vZ0N9qPETiQan1uHjKYuuFiSP+cwRXVSUYIQM9qDRlKG9zjugwO
az+B6uiR4TrgbwG+faCQwcGk9B8QbcoIb8IigwrWe3XpVaEtcsqFORX0r+tJNDoY
I0O2DOQVPKSK2N5AZzXY4IkybWTV4Yxc7rdXEO3dOOpHGKbpwFQ=
-----END RSA PRIVATE KEY-----"#;

    #[test]
    fn test_parse_ec_key() {
        for (n, key) in [SIMPLE_EC_KEY, SIMPLE_PKCS8_KEY, SIMPLE_RSA_KEY]
            .iter()
            .enumerate()
        {
            let c = Cursor::new(key.as_bytes());
            let key = super::rustls_keys::load_rustls_private_key(c);

            assert!(key.is_ok(), "at the {}-th case", n);
        }
    }
}
