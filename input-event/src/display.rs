use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

pub const MAX_MONITOR_ID_BYTES: usize = 96;
pub const MAX_MONITOR_LABEL_BYTES: usize = 96;

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct MonitorId(String);

impl MonitorId {
    pub fn new(value: String) -> Result<Self, MonitorIdError> {
        if value.is_empty() {
            return Err(MonitorIdError::Empty);
        }
        if value.len() > MAX_MONITOR_ID_BYTES {
            return Err(MonitorIdError::TooLong(value.len()));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for MonitorId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum MonitorIdError {
    #[error("monitor ID cannot be empty")]
    Empty,
    #[error("monitor ID is {0} UTF-8 bytes; maximum is {MAX_MONITOR_ID_BYTES}")]
    TooLong(usize),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MonitorDescriptor {
    pub id: MonitorId,
    pub label: String,
    pub width: u32,
    pub height: u32,
}

impl MonitorDescriptor {
    pub fn new(
        id: MonitorId,
        label: String,
        width: u32,
        height: u32,
    ) -> Result<Self, MonitorDescriptorError> {
        if label.len() > MAX_MONITOR_LABEL_BYTES {
            return Err(MonitorDescriptorError::LabelTooLong(label.len()));
        }
        if width == 0 || height == 0 {
            return Err(MonitorDescriptorError::EmptyDimensions);
        }
        Ok(Self {
            id,
            label,
            width,
            height,
        })
    }

    pub fn validate(&self) -> Result<(), MonitorDescriptorError> {
        if self.label.len() > MAX_MONITOR_LABEL_BYTES {
            return Err(MonitorDescriptorError::LabelTooLong(self.label.len()));
        }
        if self.width == 0 || self.height == 0 {
            return Err(MonitorDescriptorError::EmptyDimensions);
        }
        Ok(())
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum MonitorDescriptorError {
    #[error("monitor label is {0} UTF-8 bytes; maximum is {MAX_MONITOR_LABEL_BYTES}")]
    LabelTooLong(usize),
    #[error("monitor dimensions must be greater than zero")]
    EmptyDimensions,
}

impl<'de> Deserialize<'de> for MonitorDescriptor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawMonitorDescriptor {
            id: MonitorId,
            label: String,
            width: u32,
            height: u32,
        }

        let raw = RawMonitorDescriptor::deserialize(deserializer)?;
        Self::new(raw.id, raw.label, raw.width, raw.height).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_id_uses_utf8_byte_limits() {
        assert!(MonitorId::new("é".repeat(48)).is_ok());
        assert!(MonitorId::new("é".repeat(49)).is_err());
    }

    #[test]
    fn monitor_descriptor_rejects_invalid_labels_and_dimensions() {
        let id = MonitorId::new("DP-1".into()).unwrap();
        assert!(MonitorDescriptor::new(id.clone(), "é".repeat(48), 1920, 1080).is_ok());
        assert!(MonitorDescriptor::new(id.clone(), "é".repeat(49), 1920, 1080).is_err());
        assert!(MonitorDescriptor::new(id, "Monitor".into(), 0, 1080).is_err());
    }
}
