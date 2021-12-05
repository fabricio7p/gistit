//! The Send feature

use std::ffi::OsStr;
use std::path::Path;
use std::str;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use clap::ArgMatches;
use console::style;
use lazy_static::lazy_static;
use serde::Deserialize;
use url::Url;

use crate::clipboard::Clipboard;
use crate::dispatch::{Dispatch, GistitInner, GistitPayload, Hasheable};
use crate::encrypt::{digest_md5_multi, HashedSecret, Secret};
use crate::errors::io::IoError;
use crate::file::{name_from_path, File, FileReady};
use crate::params::{Params, SendParams};
use crate::{gistit_line_out, Error, Result};

const SERVER_IDENTIFIER_CHAR: char = '#';
lazy_static! {
    static ref GISTIT_SERVER_LOAD_URL: Url = Url::parse(
        option_env!("GISTIT_SERVER_URL")
            .unwrap_or("https://us-central1-gistit-base.cloudfunctions.net")
    )
    .expect("GISTIT_SERVER_URL env variable is not valid URL")
    .join("load")
    .expect("to join 'load' function URL");
}

/// The Send action runtime parameters
pub struct Action {
    /// The file to be sent.
    pub file: &'static OsStr,
    /// The description of this Gistit.
    pub description: Option<&'static str>,
    /// The author information.
    pub author: &'static str,
    /// The colorscheme to be displayed.
    pub theme: &'static str,
    /// The password to encrypt.
    pub secret: Option<&'static str>,
    /// The custom lifespan of a Gistit snippet.
    pub lifespan: &'static str,
    /// Whether or not to copy successfully sent gistit hash to clipboard.
    pub clipboard: bool,
    /// dry_run
    #[doc(hidden)]
    pub dry_run: bool,
}

impl<'args> Action {
    /// Parse [`ArgMatches`] into the dispatchable Send action
    ///
    /// # Errors
    ///
    /// Fails with argument errors
    pub fn from_args(
        args: &'static ArgMatches<'args>,
    ) -> Result<Box<dyn Dispatch<InnerData = Config> + 'static>> {
        let file = args.value_of_os("file").ok_or(Error::Argument)?;
        gistit_line_out!(format!(
            "{} {}",
            style("Preparing gistit:").bold(),
            style(name_from_path(Path::new(file))).green()
        ));

        Ok(Box::new(Self {
            file,
            description: args.value_of("description"),
            author: args.value_of("author").ok_or(Error::Argument)?,
            theme: args.value_of("theme").ok_or(Error::Argument)?,
            secret: args.value_of("secret"),
            lifespan: args.value_of("lifespan").ok_or(Error::Argument)?,
            clipboard: args.is_present("clipboard"),
            dry_run: args.is_present("dry-run"),
        }))
    }
}

/// The parsed/checked data that should be dispatched
pub struct Config {
    pub file: Box<dyn FileReady + Send + Sync>,
    pub params: SendParams,
    pub maybe_secret: Option<HashedSecret>,
}

#[async_trait]
impl Hasheable for Config {
    /// Hash config fields.
    /// Reads the inner file contents into a buffer and digest it into the hasher.
    /// If a secret was provided it should be digested by the hasher as well.
    ///
    /// Returns the hashed string hex encoded with an identification prefix
    ///
    /// # Errors
    ///
    /// Fails with [`std::io::Error`]
    fn hash(&self) -> String {
        let file_data = self.file.data();
        let maybe_secret_bytes = self
            .maybe_secret
            .as_ref()
            .map_or("", |s| s.to_str())
            .as_bytes();

        // Digest and collect output
        let hash = digest_md5_multi(&[file_data, maybe_secret_bytes]);
        format!("{}{}", SERVER_IDENTIFIER_CHAR, hash)
    }
}

impl Config {
    /// Trivially initialize config structure
    #[must_use]
    fn new(
        file: Box<dyn FileReady + Send + Sync>,
        params: SendParams,
        maybe_secret: Option<HashedSecret>,
    ) -> Self {
        Self {
            file,
            params,
            maybe_secret,
        }
    }

    /// Serializes this config into [`GistitPayload`]
    ///
    /// # Errors
    ///
    /// Fails with [`std::io::Error`]
    async fn into_payload(self) -> Result<GistitPayload> {
        let hash = self.hash();
        let params = self.params;
        let data = self.file.to_encoded_data();
        let file_ref = self.file.inner().await.expect("The file to be opened");

        Ok(GistitPayload {
            hash,
            author: params.author.to_owned(),
            description: params.description.map(ToOwned::to_owned),
            colorscheme: params.colorscheme.to_owned(),
            lifespan: params.lifespan,
            secret: self.maybe_secret.map(|t| t.to_str().to_owned()),
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("Check your system time")
                .as_millis()
                .to_string(),
            gistit: GistitInner {
                name: file_ref.name().clone(),
                lang: file_ref.lang().to_owned(),
                size: file_ref.size().await,
                data,
            },
        })
    }
}

/// The cloud functions response structure
#[derive(Deserialize, Debug)]
struct Response {
    success: Option<String>,
    error: Option<String>,
}

impl Response {
    fn into_inner(self) -> Result<String> {
        match self {
            Self {
                success: Some(hash),
                ..
            } => Ok(hash),
            Self {
                error: Some(error_msg),
                ..
            } => Err(Error::IO(IoError::Request(error_msg))),
            _ => unreachable!("Gistit server is unreachable"),
        }
    }
}

/// The dispatch implementation for Send action
#[async_trait]
impl Dispatch for Action {
    type InnerData = Config;
    /// Build each top level entity and run inner checks concurrently to assert valid input and
    /// output data.
    ///
    /// If all checks runs successfully, assemble the config structure to later be dispatched
    /// by [`Dispatch::dispatch`]
    async fn prepare(&self) -> Result<Self::InnerData> {
        // Check params first and exit faster if there's a invalid input
        let params = Params::from_send(self)?.check_consume()?;

        let (file, maybe_hashed_secret): (Box<dyn FileReady + Send + Sync>, Option<HashedSecret>) = {
            let path = Path::new(self.file);
            let file = File::from_path(path).await?.check_consume().await?;

            // If secret provided, hash it and encrypt file
            if let Some(secret_str) = self.secret {
                let hashed_secret = Secret::new(secret_str).check_consume()?.into_hashed()?;
                gistit_line_out!("Encrypting...");
                let encrypted_file = file.into_encrypted(secret_str).await?;
                (Box::new(encrypted_file), Some(hashed_secret))
            } else {
                (Box::new(file), None)
            }
        };
        let config = Config::new(file, params, maybe_hashed_secret);
        Ok(config)
    }
    async fn dispatch(&self, config: Self::InnerData) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        gistit_line_out!("Uploading to server...");

        let payload = config.into_payload().await?;
        let response: Response = reqwest::Client::new()
            .post(GISTIT_SERVER_LOAD_URL.to_string())
            .json(&payload)
            .send()
            .await?
            .json()
            .await?;

        let server_hash = response.into_inner()?;
        if self.clipboard {
            Clipboard::new(server_hash.clone())
                .try_into_selected()?
                .into_provider()
                .set_contents()?;
        }

        println!(
            r#"
{}:
    hash: {} {}
    url: {}{}
            "#,
            style("SUCCESS").green(),
            style(&server_hash).yellow(),
            if self.clipboard {
                style("(copied to clipboard)").italic().to_string()
            } else {
                "".to_string()
            },
            style("https://gistit.vercel.app/").cyan(),
            style(&server_hash).cyan()
        );
        Ok(())
    }
}
