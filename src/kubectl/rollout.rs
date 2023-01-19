use anyhow::Context as _;

#[derive(clap::Subcommand)]
pub enum Subcommand {
    Restart { resource: String },
}

#[derive(clap::Parser)]
pub struct Args {
    #[clap(subcommand)]
    cmd: Subcommand,
}

fn parse_resource(resource: &str) -> anyhow::Result<(&str, &str)> {
    let ind = resource
        .find('/')
        .context("resource must be specified as <kind>/<name>")?;

    let kind = match &resource[..ind] {
        "deployment" => "deployments",
        other => anyhow::bail!("unknown resource kind '{other}'"),
    };

    anyhow::ensure!(
        ind + 1 < resource.len(),
        "the resource name was not provided"
    );

    Ok((kind, &resource[ind + 1..]))
}

pub(super) async fn run(client: super::K8sClient, args: Args) -> anyhow::Result<()> {
    match args.cmd {
        Subcommand::Restart { resource } => {
            let patch = serde_json::json!({
              "spec": {
                "template": {
                  "metadata": {
                    "annotations": {
                      "boh.kubernetes.io/restartedAt": time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).context("failed to format timestamp")?,
                    }
                  }
                }
              }
            });

            let (kind, name) = parse_resource(&resource)?;

            let url = client.make_url(kind, name);

            let res = client
                .client
                .patch(url)
                .header(http::header::ACCEPT, "application/json")
                .header(http::header::CONTENT_TYPE, "application/merge-patch+json")
                .body(serde_json::to_vec(&patch).context("failed to write serialize json patch")?)
                .send()
                .await?;

            let code = res.status();
            let body = res.bytes().await?;

            if !code.is_success() {
                if let Ok(err_str) = String::from_utf8(body.into()) {
                    anyhow::bail!(err_str);
                } else {
                    anyhow::bail!("failed to retrieve error for {code}");
                }
            }

            use std::io::Write;
            let _ = std::io::stdout().write(&body);
        }
    }

    Ok(())
}
