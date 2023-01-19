use anyhow::Context as _;
pub mod rollout;

#[derive(clap::Subcommand)]
pub enum Subcommand {
    /// Manage the rollout of a resource
    Rollout(rollout::Args),
}

/// A small kubectl replacement. Note that this does not currently support
/// incluster configuration as it assumes it is being run somewhere other than
/// the target cluster
#[derive(clap::Parser)]
pub struct Args {
    /// Path to a kubeconfig file to load the cluster host and cert information from
    #[clap(long)]
    kubeconfig: camino::Utf8PathBuf,
    /// The name of the context, must be specified if there is more than one in the config
    #[clap(long)]
    cluster: Option<String>,
    /// The namespace in the cluster to operate on
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
    server: String,
    namespace: String,
    client: reqwest::Client,
}

impl K8sClient {
    fn make_url(&self, kind: &str, name: &str) -> String {
        // Note that the namespace and resource name should be
        // URL encoded, however if we ever have namespaces/names
        // that _need_ to be URL encoded, we deserve what we get
        format!(
            "{}/apis/apps/v1/namespaces/{}/{kind}/{name}?",
            self.server, self.namespace,
        )
    }
}

fn load_config(
    config_path: camino::Utf8PathBuf,
    namespace: String,
    cluster: Option<String>,
    builder: reqwest::ClientBuilder,
) -> anyhow::Result<K8sClient> {
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct ClusterDetails {
        server: String,
        #[serde(rename = "certificate-authority-data")]
        cert_data: String,
    }

    #[derive(Deserialize)]
    struct ClusterConfig {
        name: String,
        cluster: ClusterDetails,
    }

    #[derive(Deserialize)]
    struct KubeConfig {
        clusters: Vec<ClusterConfig>,
    }

    let config_data = std::fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read kubeconfig '{config_path}'"))?;
    let config: KubeConfig = serde_yaml::from_str(&config_data)
        .with_context(|| format!("failed to deserialize kubeconfig '{config_path}'"))?;

    let mut clusters = config.clusters;

    anyhow::ensure!(
        !clusters.is_empty(),
        "no clusters were defined in '{config_path}'"
    );

    let details = if let Some(cluster_name) = cluster {
        clusters
            .into_iter()
            .find_map(|cc| {
                if cc.name == cluster_name {
                    Some(cc.cluster)
                } else {
                    None
                }
            })
            .with_context(|| {
                format!("failed to find cluster '{cluster_name}' in '{config_path}'")
            })?
    } else if clusters.len() > 1 {
        let mut names = String::new();

        for cluster in clusters.into_iter().map(|cc| cc.name) {
            use std::fmt::Write;
            write!(&mut names, "{cluster}, ").unwrap();
        }

        names.pop();
        names.pop();

        anyhow::bail!("there were multiple clusters to choose from [{names}], you must specify which one to use via --cluster");
    } else {
        clusters.pop().context("unreachable")?.cluster
    };

    use base64::Engine;

    let cert = base64::engine::general_purpose::STANDARD
        .decode(details.cert_data)
        .context("failed to decode cert")?;

    let cert = openssl::x509::X509::from_pem(&cert).context("failed to load cert")?;

    let client_b = builder.add_root_certificate(reqwest::Certificate::from_der(&cert.to_der()?)?);

    Ok(K8sClient {
        server: details.server,
        namespace,
        client: client_b.build()?,
    })
}

pub async fn run(args: Args, builder: reqwest::ClientBuilder) -> anyhow::Result<()> {
    let client = load_config(args.kubeconfig, args.namespace, args.cluster, builder)?;

    match args.cmd {
        Subcommand::Rollout(args) => rollout::run(client, args).await,
    }
}
