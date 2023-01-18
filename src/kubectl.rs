pub mod rollout;

#[derive(clap::Subcommand)]
pub enum Subcommand {
    Rollout(rollout::Args),
}

#[derive(clap::Parser)]
pub struct Args {
    #[clap(long)]
    cluster: String,
    #[clap(short, long)]
    namespace: String,
    #[clap(subcommand)]
    cmd: Subcommand,
}

impl crate::Scopes for Args {
    fn scopes(&self) -> &'static [&'static str] {
        &["https://www.googleapis.com/auth/cloud-platform"]
    }
}

pub struct K8sClient {
    cluster: String,
    namespace: String,
    client: reqwest::Client,
}

impl K8sClient {
    fn make_url(&self, kind: &str, name: &str) -> String {
        // Note that the namespace and resource name should be
        // URL encoded, however if we ever have namespaces/names
        // that _need_ to be URL encoded, we deserve what we get
        format!(
            "https://{}/apis/apps/v1/namespaces/{}/{kind}/{name}?",
            self.cluster, self.namespace,
        )
    }
}

pub async fn run(args: Args, client: reqwest::Client) -> anyhow::Result<()> {
    let client = K8sClient {
        cluster: args.cluster,
        namespace: args.namespace,
        client,
    };

    match args.cmd {
        Subcommand::Rollout(args) => rollout::run(client, args).await,
    }
}
