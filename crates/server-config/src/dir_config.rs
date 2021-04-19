/*
 * Copyright 2020 Fluence Labs Limited
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */
use crate::air_interpreter_path;
use crate::defaults::{cert_dir, default_base_dir, services_basedir, stepper_basedir};

use config_utils::create_dirs;
use eyre::WrapErr;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize, Clone, Debug)]
pub struct UnresolvedDirConfig {
    /// Parent directory for all other node's directory
    #[serde(default = "default_base_dir")]
    pub base_dir: PathBuf,

    /// Directory, where all certificates are stored.
    #[serde(default)]
    pub certificate_dir: Option<PathBuf>,

    /// Base directory for resources needed by application services
    #[serde(default)]
    pub services_base_dir: Option<PathBuf>,

    /// Base directory for resources needed by application services
    #[serde(default)]
    pub stepper_base_dir: Option<PathBuf>,

    /// Path to AIR interpreter .wasm file (aquamarine.wasm)
    #[serde(default)]
    pub air_interpreter_path: Option<PathBuf>,
}

impl UnresolvedDirConfig {
    pub fn resolve(self) -> ResolvedDirConfig {
        let base = self.base_dir;
        let certificate_dir = self.certificate_dir.unwrap_or(cert_dir(&base));
        let services_base_dir = self.services_base_dir.unwrap_or(services_basedir(&base));
        let stepper_base_dir = self.stepper_base_dir.unwrap_or(stepper_basedir(&base));
        let air_interpreter_path = self
            .air_interpreter_path
            .unwrap_or(air_interpreter_path(&base));

        ResolvedDirConfig {
            base_dir: base,
            certificate_dir,
            services_base_dir,
            stepper_base_dir,
            air_interpreter_path,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedDirConfig {
    pub base_dir: PathBuf,
    pub certificate_dir: PathBuf,
    pub services_base_dir: PathBuf,
    pub stepper_base_dir: PathBuf,
    pub air_interpreter_path: PathBuf,
}

impl ResolvedDirConfig {
    pub fn create_dirs(&self) -> eyre::Result<()> {
        create_dirs(&[
            &self.base_dir,
            &self.certificate_dir,
            &self.stepper_base_dir,
        ])
        .wrap_err_with(|| format!("creating configured directories: {:#?}", self))
    }
}