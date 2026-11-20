use crate::Result;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use std::sync::OnceLock;

pub type HttpClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

pub fn client() -> Result<HttpClient> {
    static CLIENT: OnceLock<std::result::Result<HttpClient, String>> = OnceLock::new();
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
        assert!(super::client().is_ok());
    }
}
