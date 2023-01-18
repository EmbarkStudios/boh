pub mod artifact;
pub mod gcs;
pub mod kms;
pub mod kubectl;
pub mod syms;

pub trait Scopes {
    fn scopes(&self) -> &'static [&'static str];
}

pub async fn get_bearer_token(scopes: &[&str]) -> anyhow::Result<http::header::HeaderValue> {
    use anyhow::Context as _;
    use gcp::TokenProvider;
    use tame_oauth::gcp;

    let tp = gcp::TokenProviderWrapper::get_default_provider()
        .context("unable to read default credentials")?
        .context("unable to determine default credentials")?;

    let client = reqwest::Client::new();

    match tp
        .get_token(scopes)
        .context("failed to make token request")?
    {
        gcp::TokenOrRequest::Token(tok) => Ok(tok
            .try_into()
            .context("failed to convert token to header value")?),
        gcp::TokenOrRequest::Request {
            request,
            scope_hash,
            ..
        } => {
            let (parts, body) = request.into_parts();
            let uri = parts.uri.to_string();

            // Just cheat, we know this will always be POST
            let res = client
                .post(&uri)
                .headers(parts.headers)
                .body(body)
                .send()
                .await
                .context("failed to send token request")?;

            let code = res.status();

            let mut builder = http::Response::builder()
                .status(code)
                .version(res.version());

            let headers = builder
                .headers_mut()
                .context("failed to convert response headers")?;

            headers.extend(
                res.headers()
                    .into_iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );

            let buffer = res.bytes().await?;

            if !code.is_success() {
                if let Ok(err_str) = String::from_utf8(buffer.into()) {
                    anyhow::bail!(err_str);
                } else {
                    anyhow::bail!("failed to retrieve error for {code}");
                }
            }

            Ok(tp
                .parse_token_response(scope_hash, builder.body(buffer).unwrap())
                .and_then(std::convert::TryInto::try_into)
                .context("failed to convert token to header value")?)
        }
    }
}
