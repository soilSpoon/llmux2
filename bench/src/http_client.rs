use crate::Result;
use http_body_util::Full;
use hyper::{
    body::{Bytes, Incoming},
    Request, Response,
};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use std::sync::OnceLock;

type TlsClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

pub enum HttpClient {
    Http(Client<HttpConnector, Full<Bytes>>),
    Https(TlsClient),
}
impl HttpClient {
    pub async fn request(&self, request: Request<Full<Bytes>>) -> Result<Response<Incoming>> {
        Ok(match self {
            Self::Http(client) => client.request(request).await?,
            Self::Https(client) => client.request(request).await?,
        })
    }
}
pub fn client(url: &str) -> Result<HttpClient> {
    if url
        .parse::<hyper::Uri>()?
        .scheme_str()
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https"))
    {
        return tls_client().map(HttpClient::Https);
    }
    static HTTP: OnceLock<Client<HttpConnector, Full<Bytes>>> = OnceLock::new();
    Ok(HttpClient::Http(
        HTTP.get_or_init(|| Client::builder(TokioExecutor::new()).build_http())
            .clone(),
    ))
}
fn tls_client() -> Result<TlsClient> {
    static CLIENT: OnceLock<std::result::Result<TlsClient, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let connector = hyper_rustls::HttpsConnectorBuilder::new()
                .with_native_roots()
                .map_err(|error| format!("failed to load native TLS trust roots: {error}"))?
                .https_or_http()
                .enable_http1()
                .build();
            Ok(Client::builder(TokioExecutor::new()).build(connector))
        })
        .clone()
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_trust_store_builds_client() {
        assert!(super::tls_client().is_ok());
    }
    #[test]
    fn missing_ca_store_https_error() {
        if std::env::var_os("LLMUX_BENCH_TEST_EMPTY_CA_STORE").is_none() {
            return;
        }
        let error = super::client("https://127.0.0.1:1")
            .err()
            .expect("HTTPS must fail closed without native roots");
        assert!(
            error
                .to_string()
                .contains("failed to load native TLS trust roots"),
            "{error}"
        );
    }
}
