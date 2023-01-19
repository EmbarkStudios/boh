use anyhow::Context as _;
use boh::Scopes;
use clap::Parser;

#[derive(Parser)]
#[clap(author, version, about)]
enum Args {
    Artifact(boh::artifact::Args),
    #[clap(subcommand)]
    Gcs(boh::gcs::Args),
    #[clap(subcommand)]
    Kms(boh::kms::Args),
    Kubectl(boh::kubectl::Args),
    Syms(boh::syms::Args),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let scopes = match &args {
        Args::Artifact(a) => a.scopes(),
        Args::Gcs(a) => a.scopes(),
        Args::Kms(a) => a.scopes(),
        Args::Kubectl(a) => a.scopes(),
        Args::Syms(a) => a.scopes(),
    };

    // Get a token for the default credentials on the system
    let auth_token = boh::get_bearer_token(scopes).await?;

    let hm = {
        let mut hm = reqwest::header::HeaderMap::new();
        hm.insert(http::header::AUTHORIZATION, auth_token.clone());
        hm
    };

    let client_builder = reqwest::Client::builder().default_headers(hm);

    match args {
        Args::Artifact(gcs) => boh::artifact::run(gcs, client_builder).await?,
        Args::Gcs(gcs) => boh::gcs::run(gcs, client_builder).await?,
        Args::Kms(kms) => boh::kms::run(kms, client_builder).await?,
        Args::Kubectl(kube) => boh::kubectl::run(kube, client_builder).await?,
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
