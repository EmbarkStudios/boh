use anyhow::Context as _;
use boh::Scopes;
use clap::Parser;

#[derive(Parser)]
#[clap(author, version, about)]
enum Args {
    #[clap(subcommand)]
    Kms(boh::kms::Args),
    #[clap(subcommand)]
    Gcs(boh::gcs::Args),
    Artifact(boh::artifact::Args),
    Syms(boh::syms::Args),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let scopes = match &args {
        Args::Kms(a) => a.scopes(),
        Args::Gcs(a) => a.scopes(),
        Args::Artifact(a) => a.scopes(),
        Args::Syms(a) => a.scopes(),
    };

    // Get a token for the default credentials on the system
    let auth_token = boh::get_bearer_token(scopes).await?;

    let hm = {
        let mut hm = reqwest::header::HeaderMap::new();
        hm.insert(http::header::AUTHORIZATION, auth_token.clone());
        hm
    };

    let client = reqwest::Client::builder()
        .default_headers(hm)
        .build()
        .context("failed to build client")?;

    match args {
        Args::Kms(kms) => boh::kms::run(kms, client).await?,
        Args::Gcs(gcs) => boh::gcs::run(gcs, client).await?,
        Args::Artifact(gcs) => boh::artifact::run(gcs, client).await?,
        Args::Syms(syms) => {
            let hm = {
                let mut hm = reqwest::header::HeaderMap::new();
                hm.insert(http::header::AUTHORIZATION, auth_token);
                hm
            };

            let client = reqwest::blocking::Client::builder()
                .default_headers(hm)
                .build()
                .context("failed to build client")?;

            boh::syms::run(syms, client).await?
        }
    }

    Ok(())
}
