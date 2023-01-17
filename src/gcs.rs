pub mod cp;
mod util;

/// Performs GCS operations
#[derive(clap::Subcommand)]
pub enum Args {
    Cp(cp::Args),
}

impl crate::Scopes for Args {
    fn scopes(&self) -> &'static [&'static str] {
        &["https://www.googleapis.com/auth/devstorage.full_control"]
    }
}

pub async fn run(args: Args, client: reqwest::Client) -> anyhow::Result<()> {
    let rctx = util::RequestContext {
        client,
        obj: tame_gcs::objects::Object::default(),
    };

    match args {
        Args::Cp(cp) => cp::run(&rctx, cp).await?,
    }

    Ok(())
}
