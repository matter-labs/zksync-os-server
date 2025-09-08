#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum ZKsyncOSVersion {
    V0_0_21,
}

impl ZKsyncOSVersion {
    pub fn latest() -> Self {
        Self::V0_0_21
    }
}

impl From<ZKsyncOSVersion> for semver::Version {
    fn from(version: ZKsyncOSVersion) -> Self {
        match version {
            ZKsyncOSVersion::V0_0_21 => semver::Version::new(0, 0, 21),
        }
    }
}

impl TryFrom<semver::Version> for ZKsyncOSVersion {
    type Error = anyhow::Error;

    fn try_from(version: semver::Version) -> Result<Self, Self::Error> {
        match version.to_string().as_str() {
            "0.0.21" => Ok(ZKsyncOSVersion::V0_0_21),
            _ => Err(anyhow::anyhow!("Unsupported ZKsync OS version: {version}")),
        }
    }
}
