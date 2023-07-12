use anyhow::Context as _;
use async_scoped::TokioScope as Scope;
use clap::Parser;
use nu_ansi_term::Color;
use reqwest::Client;

#[derive(serde::Deserialize, Clone)]
struct TaggedImage {
    repo: Option<String>,
    name: String,
    tag: String,
}

use std::fmt;

impl fmt::Display for TaggedImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.name, self.tag)
    }
}

#[derive(serde::Deserialize)]
struct OciConfig {
    architecture: String,
    os: String,
    #[serde(default)]
    variant: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OciManifestConfigRef {
    digest: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OciManifest {
    config: OciManifestConfigRef,
}

#[inline]
fn calc_digest(data: &[u8]) -> String {
    use std::fmt::Write;
    let digest = ring::digest::digest(&ring::digest::SHA256, data);

    let mut ds = String::new();
    ds.push_str("sha256:");

    for b in digest.as_ref() {
        write!(&mut ds, "{b:02x}").unwrap();
    }

    ds
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct OciManifestList {
    schema_version: u8,
    media_type: String,
    manifests: Vec<OciManifestListManifest>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct OciManifestListPlatform {
    architecture: String,
    os: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    variant: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct OciManifestListManifest {
    media_type: String,
    digest: String,
    size: u64,
    platform: OciManifestListPlatform,
}

const MANIFEST_LIST_MIME: &str = "application/vnd.oci.image.index.v1+json";

async fn get_bytes(req: reqwest::RequestBuilder) -> anyhow::Result<bytes::Bytes> {
    let res = req.send().await?;

    let code = res.status();
    let buffer = res.bytes().await?;

    if code.is_success() {
        Ok(buffer)
    } else if let Ok(err_str) = String::from_utf8(buffer.into()) {
        anyhow::bail!(err_str);
    } else {
        anyhow::bail!("failed to retrieve error for {code}");
    }
}

async fn push_manifest(
    client: &Client,
    region: &str,
    repo: &Option<String>,
    sources: Vec<TaggedImage>,
    targets: &[TaggedImage],
) -> anyhow::Result<()> {
    // 1. Retrieve the image manifest for each of the source images
    // https://docs.docker.com/registry/spec/api/#get-manifest
    // SAFETY: We must not forget the future...which is fine since we don't :p
    let (_, manifests) = unsafe {
        Scope::scope_and_collect(|s| {
            for ti in sources {
                s.spawn(async move {
                    let src_repo = ti.repo.as_ref().or(repo.as_ref()).context("source repo not set")?;
                    let manifest_raw = get_bytes(client.get(format!("https://{region}-docker.pkg.dev/v2/{src_repo}/{}/manifests/{}", ti.name, ti.tag))).await?;

                    let manifest: OciManifest = serde_json::from_slice(&manifest_raw)?;
                    let manifest_digest = calc_digest(&manifest_raw);

                    // 2. Unfortunately, the os/arch information is not stored in the
                    // manifest, but rather the image config, so retrieve that for
                    // each source as well
                    // https://docs.docker.com/registry/spec/api/#get-blob
                    let config_raw = get_bytes(client.get(format!("https://{region}-docker.pkg.dev/v2/{src_repo}/{}/blobs/{}", ti.name, manifest.config.digest))).await?;

                    // Validate the config actually is correct
                    let config_digest = calc_digest(&config_raw);

                    anyhow::ensure!(config_digest == manifest.config.digest, "config digest does not match, blob was supposedly '{}', but was calculated as '{config_digest}'", manifest.config.digest);

                    let config: OciConfig = serde_json::from_slice(&config_raw)?;

                    let list = OciManifestListManifest {
                        media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
                        digest: manifest_digest.clone(),
                        size: manifest_raw.len() as _,
                        platform: OciManifestListPlatform {
                            architecture: config.architecture,
                            os: config.os,
                            variant: config.variant,
                        },
                    };

                    // 3. If the source manifest has a different repo or name than the target
                    // image we need to copy the manifest there first
                    for target in targets {
                        let tar_repo = target
                            .repo
                            .as_ref()
                            .or(repo.as_ref())
                            .context("target repo not set")?;
                        let tar_name = target.name.as_str();
                        if tar_repo != src_repo || tar_name != ti.name {
                            let mut rb = client.put(format!(
                                "https://{region}-docker.pkg.dev/v2/{tar_repo}/{tar_name}/manifests/{manifest_digest}"
                            ));
                            rb = rb.header(http::header::CONTENT_TYPE, "application/vnd.oci.image.manifest.v1+json");
                            rb = rb.body(manifest_raw.clone());

                            get_bytes(rb).await?;
                        }
                    }

                    Ok(list)
                });
            }
        }).await
    };

    let manifests = manifests
        .into_iter()
        .map(|m| m.unwrap())
        .collect::<anyhow::Result<Vec<_>>>()?;

    let manifest_list = serde_json::to_vec(&OciManifestList {
        schema_version: 2,
        media_type: MANIFEST_LIST_MIME.to_owned(),
        manifests,
    })?;

    for target in targets {
        let tar_repo = target
            .repo
            .as_ref()
            .or(repo.as_ref())
            .context("target repo not set")?;
        let mut rb = client.put(format!(
            "https://{region}-docker.pkg.dev/v2/{tar_repo}/{}/manifests/{}",
            target.name, target.tag
        ));
        rb = rb.header(http::header::CONTENT_TYPE, MANIFEST_LIST_MIME);

        rb = rb.body(manifest_list.clone());
        get_bytes(rb).await?;
    }

    Ok(())
}

async fn add_tag(
    client: &Client,
    region: &str,
    repo: &Option<String>,
    source: TaggedImage,
    target: TaggedImage,
) -> anyhow::Result<()> {
    let src_repo = source
        .repo
        .as_ref()
        .or(repo.as_ref())
        .context("source repo not set")?;
    let tar_repo = target
        .repo
        .as_ref()
        .or(repo.as_ref())
        .context("target repo not set")?;

    // The docker HTTP API doesn't have a dedicated way to add tags to an existing
    // image, so we just download the manifest and reupload it with the new tag
    let manifest_resp = client
        .get(format!(
            "https://{region}-docker.pkg.dev/v2/{src_repo}/{}/manifests/{}",
            source.name, source.tag
        ))
        .send()
        .await?;

    let content_type = manifest_resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .context("couldn't extract content type")?
        .clone();

    let manifest_raw = manifest_resp.error_for_status()?.bytes().await?;

    let mut rb = client.put(format!(
        "https://{region}-docker.pkg.dev/v2/{tar_repo}/{}/manifests/{}",
        target.name, target.tag
    ));
    rb = rb.header(http::header::CONTENT_TYPE, content_type);
    rb = rb.body(manifest_raw);

    get_bytes(rb).await?;
    Ok(())
}

/// Tags are creates manifests lists to one or more artifact registry regions
/// when provided a configuration via stdin
#[derive(Parser)]
pub struct Args {
    /// Regions to operate on
    #[clap(short, long)]
    regions: Vec<String>,
}

impl crate::Scopes for Args {
    fn scopes(&self) -> &'static [&'static str] {
        &["https://www.googleapis.com/auth/cloud-platform"]
    }
}

#[derive(serde::Deserialize)]
struct Manifests {
    /// Base repo, used if repo is not set on the individual manifests
    repo: Option<String>,
    items: Vec<Item>,
}

#[derive(serde::Deserialize, Clone)]
struct Item {
    /// If one source, the image is retagged, if multiple sources, they are combined
    /// into a single manifest list
    sources: Vec<TaggedImage>,
    /// The manifest list to be created
    targets: Vec<TaggedImage>,
}

pub async fn run(args: Args, client: reqwest::ClientBuilder) -> anyhow::Result<()> {
    let client = client.build()?;

    let mc = {
        use std::io::Read;
        let mut mc = String::new();
        std::io::stdin()
            .read_to_string(&mut mc)
            .context("failed to read stdin")?;

        mc
    };

    let to_push: Manifests =
        serde_json::from_str(&mc).context("failed to parse manifests from stdin")?;

    // SAFETY: We must not forget the future...which is fine since we don't :p
    let (_, pushed) = unsafe {
        Scope::scope_and_collect(|s| {
            let repo = to_push.repo;
            for region in args.regions {
                println!("region: {}", Color::Cyan.paint(&region));
                let state = std::sync::Arc::new((client.clone(), region, repo.clone()));
                for mut tp in to_push.items.iter().cloned() {
                    let targets = {
                        let mut t = String::new();

                        use std::fmt::Write;
                        for tar in &tp.targets {
                            write!(&mut t, "{tar}").unwrap();
                            t.push_str(", ");
                        }

                        t.pop();
                        t.pop();
                        t
                    };

                    if tp.sources.len() == 1 {
                        println!(
                            "  tagging {} <= {}",
                            Color::Green.paint(targets),
                            Color::Blue.paint(tp.sources[0].to_string())
                        );
                    } else {
                        println!("  manifest list [{}]:", Color::Green.paint(targets));
                        for src in &tp.sources {
                            println!("    {}", Color::Blue.paint(src.to_string()));
                        }
                    }

                    let state = state.clone();
                    s.spawn(async move {
                        if tp.sources.len() == 1 {
                            add_tag(
                                &state.0,
                                &state.1,
                                &state.2,
                                tp.sources.pop().unwrap(),
                                tp.targets.pop().unwrap(),
                            )
                            .await
                        } else {
                            push_manifest(&state.0, &state.1, &state.2, tp.sources, &tp.targets)
                                .await
                        }
                    });
                }
            }
        })
        .await
    };

    pushed
        .into_iter()
        .map(|m| m.unwrap())
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(())
}
