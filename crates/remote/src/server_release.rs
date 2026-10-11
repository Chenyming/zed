use std::{env::VarError, sync::LazyLock};

use anyhow::{Context as _, Result};
use release_channel::ReleaseChannel;
use semver::Version;

// Keep the selection fixed for the lifetime of the connection pool. A running
// process must not reuse a connection installed with a different override.
static VERSION_OVERRIDE: LazyLock<Result<Option<Version>, String>> = LazyLock::new(|| {
    let runtime = match std::env::var("ZED_REMOTE_SERVER_VERSION") {
        Ok(value) => Some(value),
        Err(VarError::NotPresent) => None,
        Err(error) => return Err(format!("Invalid ZED_REMOTE_SERVER_VERSION: {error}")),
    };
    let version =
        parse_version_override(runtime.as_deref(), option_env!("ZED_REMOTE_SERVER_VERSION"))
            .map_err(|error| format!("{error:#}"))?;
    if let Some(version) = &version {
        log::warn!(
            "Using official stable remote server {version} via ZED_REMOTE_SERVER_VERSION; \
             compatibility with this client is not guaranteed"
        );
    }
    Ok(version)
});

fn parse_version_override(runtime: Option<&str>, bundled: Option<&str>) -> Result<Option<Version>> {
    let Some(value) = runtime
        .or(bundled)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let version = Version::parse(value).context(
        "ZED_REMOTE_SERVER_VERSION must be an exact stable version such as 1.20.2, not latest",
    )?;
    anyhow::ensure!(
        version.pre.is_empty() && version.build.is_empty(),
        "ZED_REMOTE_SERVER_VERSION must not contain prerelease or build metadata"
    );
    Ok(Some(version))
}

pub(crate) struct RemoteServerRelease {
    pub channel: ReleaseChannel,
    pub version: Version,
    pub overridden: bool,
}

impl RemoteServerRelease {
    pub fn for_client(channel: ReleaseChannel, version: Version) -> Result<Self> {
        let version_override = VERSION_OVERRIDE
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(Self::resolve(channel, version, version_override.clone()))
    }

    fn resolve(
        channel: ReleaseChannel,
        version: Version,
        version_override: Option<Version>,
    ) -> Self {
        match version_override {
            Some(version) => Self {
                channel: ReleaseChannel::Stable,
                version,
                overridden: true,
            },
            None => Self {
                channel,
                version,
                overridden: false,
            },
        }
    }

    pub fn accepts_version_output(&self, output: &str) -> bool {
        if !self.overridden {
            return true;
        }
        let Some(output) = output.lines().next_back() else {
            return false;
        };
        Version::parse(output.trim()).is_ok_and(|mut version| {
            version.build = semver::BuildMetadata::EMPTY;
            version == self.version
        })
    }

    pub async fn verify_download(
        &self,
        version_output: impl Future<Output = Result<String>>,
    ) -> Result<()> {
        if !self.overridden {
            return Ok(());
        }
        let output = version_output
            .await
            .context("Checking downloaded remote server version")?;
        anyhow::ensure!(
            self.accepts_version_output(&output),
            "Downloaded remote server reports {:?}, expected stable {} selected by ZED_REMOTE_SERVER_VERSION",
            output.trim(),
            self.version,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_override_precedence_and_validation() {
        let pinned = Some(Version::new(1, 20, 2));
        assert_eq!(
            parse_version_override(None, Some("1.20.2")).unwrap(),
            pinned
        );
        assert_eq!(
            parse_version_override(Some("1.20.2"), Some("1.19.0")).unwrap(),
            pinned
        );
        assert_eq!(
            parse_version_override(Some(""), Some("1.20.2")).unwrap(),
            None
        );
        assert_eq!(parse_version_override(None, None).unwrap(), None);
        for value in [
            "latest",
            "1.20",
            "../1.20.2",
            "1.20.2-beta.1",
            "1.20.2+custom",
        ] {
            assert!(
                parse_version_override(Some(value), None).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn override_selects_stable_without_changing_client_version() {
        let client_version = Version::parse("1.21.0+stable.custom").unwrap();
        for channel in [
            ReleaseChannel::Dev,
            ReleaseChannel::Nightly,
            ReleaseChannel::Preview,
            ReleaseChannel::Stable,
        ] {
            let default = RemoteServerRelease::resolve(channel, client_version.clone(), None);
            assert_eq!(default.channel, channel);
            assert_eq!(default.version, client_version);
            assert!(!default.overridden);

            let pinned = RemoteServerRelease::resolve(
                channel,
                client_version.clone(),
                Some(Version::new(1, 20, 2)),
            );
            assert_eq!(pinned.channel, ReleaseChannel::Stable);
            assert_eq!(pinned.version, Version::new(1, 20, 2));
            assert!(pinned.overridden);
        }
    }

    #[test]
    fn overridden_binary_must_report_selected_version() {
        let release = RemoteServerRelease::resolve(
            ReleaseChannel::Dev,
            Version::new(1, 21, 0),
            Some(Version::new(1, 20, 2)),
        );
        assert!(release.accepts_version_output("1.20.2\n"));
        assert!(release.accepts_version_output("shell banner\n1.20.2+stable.commit\n"));
        for output in ["1.21.0\n", "1.20.1\n", "1.20.2-beta.1\n", "commit-sha", ""] {
            assert!(!release.accepts_version_output(output), "{output}");
        }
    }

    #[test]
    fn downloaded_override_is_verified_before_use() {
        let release = RemoteServerRelease::resolve(
            ReleaseChannel::Dev,
            Version::new(1, 21, 0),
            Some(Version::new(1, 20, 2)),
        );
        for (output, accepted) in [("1.20.2", true), ("1.21.0", false), ("", false)] {
            let result = futures::executor::block_on(
                release.verify_download(async { Ok(output.to_string()) }),
            );
            assert_eq!(result.is_ok(), accepted);
        }
        assert!(
            futures::executor::block_on(
                release.verify_download(async { anyhow::bail!("cannot execute server") })
            )
            .is_err()
        );
        let default =
            RemoteServerRelease::resolve(ReleaseChannel::Dev, Version::new(1, 21, 0), None);
        assert!(
            futures::executor::block_on(default.verify_download(async {
                anyhow::bail!("default must not invoke extra version check")
            }))
            .is_ok()
        );
    }
}
