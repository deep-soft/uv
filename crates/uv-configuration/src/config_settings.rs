use std::{
    collections::{BTreeMap, btree_map::Entry},
    str::FromStr,
};
use uv_cache_key::CacheKeyHasher;
use uv_normalize::PackageName;

#[derive(Debug, Clone)]
pub struct ConfigSettingEntry {
    /// The key of the setting. For example, given `key=value`, this would be `key`.
    key: String,
    /// The value of the setting. For example, given `key=value`, this would be `value`.
    value: String,
}

impl FromStr for ConfigSettingEntry {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some((key, value)) = s.split_once('=') else {
            return Err(format!(
                "Invalid config setting: {s} (expected `KEY=VALUE`)"
            ));
        };
        Ok(Self {
            key: key.trim().to_string(),
            value: value.trim().to_string(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ConfigSettingPackageEntry {
    /// The package name to apply the setting to.
    package: PackageName,
    /// The config setting entry.
    setting: ConfigSettingEntry,
}

impl FromStr for ConfigSettingPackageEntry {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some((package_str, config_str)) = s.split_once(':') else {
            return Err(format!(
                "Invalid config setting: {s} (expected `PACKAGE:KEY=VALUE`)"
            ));
        };

        let package = PackageName::from_str(package_str.trim())
            .map_err(|e| format!("Invalid package name: {e}"))?;
        let setting = ConfigSettingEntry::from_str(config_str)?;

        Ok(Self { package, setting })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(untagged))]
enum ConfigSettingValue {
    /// The value consists of a single string.
    String(String),
    /// The value consists of a list of strings.
    List(Vec<String>),
}

impl serde::Serialize for ConfigSettingValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            ConfigSettingValue::String(value) => serializer.serialize_str(value),
            ConfigSettingValue::List(values) => serializer.collect_seq(values.iter()),
        }
    }
}

impl<'de> serde::Deserialize<'de> for ConfigSettingValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ConfigSettingValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a string or list of strings")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(ConfigSettingValue::String(value.to_string()))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element()? {
                    values.push(value);
                }
                Ok(ConfigSettingValue::List(values))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// Settings to pass to a PEP 517 build backend, structured as a map from (string) key to string or
/// list of strings.
///
/// See: <https://peps.python.org/pep-0517/#config-settings>
#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ConfigSettings(BTreeMap<String, ConfigSettingValue>);

impl FromIterator<ConfigSettingEntry> for ConfigSettings {
    fn from_iter<T: IntoIterator<Item = ConfigSettingEntry>>(iter: T) -> Self {
        let mut config = BTreeMap::default();
        for entry in iter {
            match config.entry(entry.key) {
                Entry::Vacant(vacant) => {
                    vacant.insert(ConfigSettingValue::String(entry.value));
                }
                Entry::Occupied(mut occupied) => match occupied.get_mut() {
                    ConfigSettingValue::String(existing) => {
                        let existing = existing.clone();
                        occupied.insert(ConfigSettingValue::List(vec![existing, entry.value]));
                    }
                    ConfigSettingValue::List(existing) => {
                        existing.push(entry.value);
                    }
                },
            }
        }
        Self(config)
    }
}

impl ConfigSettings {
    /// Returns the number of settings in the configuration.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if the configuration contains no settings.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Convert the settings to a string that can be passed directly to a PEP 517 build backend.
    pub fn escape_for_python(&self) -> String {
        serde_json::to_string(self).expect("Failed to serialize config settings")
    }

    /// Merge two sets of config settings, with the values in `self` taking precedence.
    #[must_use]
    pub fn merge(self, other: ConfigSettings) -> ConfigSettings {
        let mut config = self.0;
        for (key, value) in other.0 {
            match config.entry(key) {
                Entry::Vacant(vacant) => {
                    vacant.insert(value);
                }
                Entry::Occupied(mut occupied) => match occupied.get_mut() {
                    ConfigSettingValue::String(existing) => {
                        let existing = existing.clone();
                        match value {
                            ConfigSettingValue::String(value) => {
                                occupied.insert(ConfigSettingValue::List(vec![existing, value]));
                            }
                            ConfigSettingValue::List(mut values) => {
                                values.insert(0, existing);
                                occupied.insert(ConfigSettingValue::List(values));
                            }
                        }
                    }
                    ConfigSettingValue::List(existing) => match value {
                        ConfigSettingValue::String(value) => {
                            existing.push(value);
                        }
                        ConfigSettingValue::List(values) => {
                            existing.extend(values);
                        }
                    },
                },
            }
        }
        Self(config)
    }
}

impl uv_cache_key::CacheKey for ConfigSettings {
    fn cache_key(&self, state: &mut CacheKeyHasher) {
        for (key, value) in &self.0 {
            key.cache_key(state);
            match value {
                ConfigSettingValue::String(value) => value.cache_key(state),
                ConfigSettingValue::List(values) => values.cache_key(state),
            }
        }
    }
}

impl serde::Serialize for ConfigSettings {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in &self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de> serde::Deserialize<'de> for ConfigSettings {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ConfigSettings;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a map from string to string or list of strings")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut config = BTreeMap::default();
                while let Some((key, value)) = map.next_entry()? {
                    config.insert(key, value);
                }
                Ok(ConfigSettings(config))
            }
        }

        deserializer.deserialize_map(Visitor)
    }
}

/// Settings to pass to PEP 517 build backends on a per-package basis.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PackageConfigSettings(BTreeMap<PackageName, ConfigSettings>);

impl FromIterator<ConfigSettingPackageEntry> for PackageConfigSettings {
    fn from_iter<T: IntoIterator<Item = ConfigSettingPackageEntry>>(iter: T) -> Self {
        let mut package_configs: BTreeMap<PackageName, Vec<ConfigSettingEntry>> = BTreeMap::new();

        for entry in iter {
            package_configs
                .entry(entry.package)
                .or_default()
                .push(entry.setting);
        }

        let configs = package_configs
            .into_iter()
            .map(|(package, entries)| (package, entries.into_iter().collect()))
            .collect();

        Self(configs)
    }
}

impl PackageConfigSettings {
    /// Returns the config settings for a specific package, if any.
    pub fn get(&self, package: &PackageName) -> Option<&ConfigSettings> {
        self.0.get(package)
    }

    /// Returns `true` if there are no package-specific settings.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Merge two sets of package config settings, with the values in `self` taking precedence.
    #[must_use]
    pub fn merge(mut self, other: PackageConfigSettings) -> PackageConfigSettings {
        for (package, settings) in other.0 {
            match self.0.entry(package) {
                Entry::Vacant(vacant) => {
                    vacant.insert(settings);
                }
                Entry::Occupied(mut occupied) => {
                    let merged = occupied.get().clone().merge(settings);
                    occupied.insert(merged);
                }
            }
        }
        self
    }
}

impl uv_cache_key::CacheKey for PackageConfigSettings {
    fn cache_key(&self, state: &mut CacheKeyHasher) {
        for (package, settings) in &self.0 {
            package.to_string().cache_key(state);
            settings.cache_key(state);
        }
    }
}

impl serde::Serialize for PackageConfigSettings {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in &self.0 {
            map.serialize_entry(&key.to_string(), value)?;
        }
        map.end()
    }
}

impl<'de> serde::Deserialize<'de> for PackageConfigSettings {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = PackageConfigSettings;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a map from package name to config settings")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut config = BTreeMap::default();
                while let Some((key, value)) = map.next_entry::<String, ConfigSettings>()? {
                    let package = PackageName::from_str(&key).map_err(|e| {
                        serde::de::Error::custom(format!("Invalid package name: {e}"))
                    })?;
                    config.insert(package, value);
                }
                Ok(PackageConfigSettings(config))
            }
        }

        deserializer.deserialize_map(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_config_settings() {
        let settings: ConfigSettings = vec![
            ConfigSettingEntry {
                key: "key".to_string(),
                value: "value".to_string(),
            },
            ConfigSettingEntry {
                key: "key".to_string(),
                value: "value2".to_string(),
            },
            ConfigSettingEntry {
                key: "list".to_string(),
                value: "value3".to_string(),
            },
            ConfigSettingEntry {
                key: "list".to_string(),
                value: "value4".to_string(),
            },
        ]
        .into_iter()
        .collect();
        assert_eq!(
            settings.0.get("key"),
            Some(&ConfigSettingValue::List(vec![
                "value".to_string(),
                "value2".to_string()
            ]))
        );
        assert_eq!(
            settings.0.get("list"),
            Some(&ConfigSettingValue::List(vec![
                "value3".to_string(),
                "value4".to_string()
            ]))
        );
    }

    #[test]
    fn escape_for_python() {
        let mut settings = ConfigSettings::default();
        settings.0.insert(
            "key".to_string(),
            ConfigSettingValue::String("value".to_string()),
        );
        settings.0.insert(
            "list".to_string(),
            ConfigSettingValue::List(vec!["value1".to_string(), "value2".to_string()]),
        );
        assert_eq!(
            settings.escape_for_python(),
            r#"{"key":"value","list":["value1","value2"]}"#
        );

        let mut settings = ConfigSettings::default();
        settings.0.insert(
            "key".to_string(),
            ConfigSettingValue::String("Hello, \"world!\"".to_string()),
        );
        settings.0.insert(
            "list".to_string(),
            ConfigSettingValue::List(vec!["'value1'".to_string()]),
        );
        assert_eq!(
            settings.escape_for_python(),
            r#"{"key":"Hello, \"world!\"","list":["'value1'"]}"#
        );

        let mut settings = ConfigSettings::default();
        settings.0.insert(
            "key".to_string(),
            ConfigSettingValue::String("val\\1 {}value".to_string()),
        );
        assert_eq!(settings.escape_for_python(), r#"{"key":"val\\1 {}value"}"#);
    }

    #[test]
    fn parse_config_setting_package_entry() {
        // Test valid parsing
        let entry = ConfigSettingPackageEntry::from_str("numpy:editable_mode=compat").unwrap();
        assert_eq!(entry.package.as_ref(), "numpy");
        assert_eq!(entry.setting.key, "editable_mode");
        assert_eq!(entry.setting.value, "compat");

        // Test with package name containing hyphens
        let entry = ConfigSettingPackageEntry::from_str("my-package:some_key=value").unwrap();
        assert_eq!(entry.package.as_ref(), "my-package");
        assert_eq!(entry.setting.key, "some_key");
        assert_eq!(entry.setting.value, "value");

        // Test with spaces around values
        let entry = ConfigSettingPackageEntry::from_str("  numpy : key = value  ").unwrap();
        assert_eq!(entry.package.as_ref(), "numpy");
        assert_eq!(entry.setting.key, "key");
        assert_eq!(entry.setting.value, "value");
    }

    #[test]
    fn collect_config_settings_package() {
        let settings: PackageConfigSettings = vec![
            ConfigSettingPackageEntry::from_str("numpy:editable_mode=compat").unwrap(),
            ConfigSettingPackageEntry::from_str("numpy:another_key=value").unwrap(),
            ConfigSettingPackageEntry::from_str("scipy:build_option=fast").unwrap(),
        ]
        .into_iter()
        .collect();

        let numpy_settings = settings
            .get(&PackageName::from_str("numpy").unwrap())
            .unwrap();
        assert_eq!(
            numpy_settings.0.get("editable_mode"),
            Some(&ConfigSettingValue::String("compat".to_string()))
        );
        assert_eq!(
            numpy_settings.0.get("another_key"),
            Some(&ConfigSettingValue::String("value".to_string()))
        );

        let scipy_settings = settings
            .get(&PackageName::from_str("scipy").unwrap())
            .unwrap();
        assert_eq!(
            scipy_settings.0.get("build_option"),
            Some(&ConfigSettingValue::String("fast".to_string()))
        );
    }
}
