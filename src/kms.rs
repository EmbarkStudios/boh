use anyhow::Context as _;
use clap::Parser;
use reqwest::Client;
use std::path::PathBuf;

#[derive(Parser)]
pub struct KeyInfo {
    /// The name of the key to use
    #[arg(long)]
    key: String,
    /// The name of the keyring where the key is located
    #[arg(long)]
    keyring: String,
    /// The location of the keyring
    #[arg(long)]
    location: String,
    /// The project the keyring is located in
    #[arg(long)]
    project: String,
}

/// Encrypts the contents of one file into another file
#[derive(Parser)]
pub struct Encrypt {
    #[clap(flatten)]
    key_info: KeyInfo,
    /// The path of the plaintext file to read and encrypt
    #[arg(long)]
    input: PathBuf,
    /// The path where the encrypted data will be written
    #[arg(long)]
    output: PathBuf,
}

/// Decrypts the contents of one file into another file
#[derive(Parser)]
pub struct Decrypt {
    #[clap(flatten)]
    key_info: KeyInfo,
    /// The path of the encrypted file to read and decrypt
    #[arg(long)]
    input: PathBuf,
    /// The path where the decrypted data will be written
    #[arg(long)]
    output: PathBuf,
}

/// Performs encryption or decryption
#[derive(clap::Subcommand)]
pub enum Args {
    Encrypt(Encrypt),
    Decrypt(Decrypt),
}

impl crate::Scopes for Args {
    fn scopes(&self) -> &'static [&'static str] {
        &["https://www.googleapis.com/auth/cloudkms"]
    }
}

mod base64 {
    use base64::Engine as _;
    use serde::{de, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        serializer.serialize_str(&encoded)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = <&str>::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .map_err(de::Error::custom)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Plaintext {
    #[serde(with = "base64")]
    plaintext: Vec<u8>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Ciphertext {
    #[serde(with = "base64")]
    ciphertext: Vec<u8>,
}

/// <https://cloud.google.com/kms/docs/encrypt-decrypt#encrypt>
async fn encrypt(args: Encrypt, client: reqwest::Client) -> anyhow::Result<()> {
    let url = format!("https://cloudkms.googleapis.com/v1/projects/{project}/locations/{location}/keyRings/{keyring}/cryptoKeys/{key}:encrypt",
        project = args.key_info.project,
        location = args.key_info.location,
        keyring = args.key_info.keyring,
        key = args.key_info.key,
    );

    let data = std::fs::read(&args.input)
        .with_context(|| format!("unable to read {}", args.input.display()))?;

    let response = client
        .post(&url)
        .json(&Plaintext { plaintext: data })
        .send()
        .await
        .context("failed to send request")?
        .error_for_status()
        .context("encryption request failed")?;

    let body: Ciphertext = response
        .json()
        .await
        .context("failed to deserialize body")?;

    std::fs::write(&args.output, &body.ciphertext).with_context(|| {
        format!(
            "failed to write encrypted data to {}",
            args.output.display()
        )
    })?;

    Ok(())
}

/// <https://cloud.google.com/kms/docs/encrypt-decrypt#decrypt>
async fn decrypt(args: Decrypt, client: reqwest::Client) -> anyhow::Result<()> {
    let url = format!("https://cloudkms.googleapis.com/v1/projects/{project}/locations/{location}/keyRings/{keyring}/cryptoKeys/{key}:decrypt",
        project = args.key_info.project,
        location = args.key_info.location,
        keyring = args.key_info.keyring,
        key = args.key_info.key,
    );

    let data = std::fs::read(&args.input)
        .with_context(|| format!("unable to read {}", args.input.display()))?;

    let response = client
        .post(&url)
        .json(&Ciphertext { ciphertext: data })
        .send()
        .await
        .context("failed to send request")?
        .error_for_status()
        .context("decryption request failed")?;

    let body: Plaintext = response
        .json()
        .await
        .context("failed to deserialize body")?;

    anyhow::ensure!(
        !body.plaintext.is_empty(),
        "Decryption resulted in an empty plaintext"
    );

    std::fs::write(&args.output, &body.plaintext).with_context(|| {
        format!(
            "failed to write decrypted data to {}",
            args.output.display()
        )
    })?;

    Ok(())
}

pub async fn run(args: Args, client: Client) -> anyhow::Result<()> {
    match args {
        Args::Encrypt(args) => encrypt(args, client).await?,
        Args::Decrypt(args) => decrypt(args, client).await?,
    }

    Ok(())
}
