// Copyright (c) 2016 Chef Software Inc. and/or applicable contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std;
use std::fmt;
use std::io::BufReader;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::result;

use hcore;
use hcore::service::ServiceGroup;
use serde::{Serialize, Serializer};

use super::health;
use error::Result;
use manager::service::ServiceConfig;
use supervisor::RuntimeConfig;
use templating::Template;
use util;

pub const HOOK_PERMISSIONS: u32 = 0o755;
static LOGKEY: &'static str = "HK";

#[derive(Debug, Copy, Clone)]
pub struct ExitCode(i32);

impl Default for ExitCode {
    fn default() -> ExitCode {
        ExitCode(-1)
    }
}

pub trait Hook: fmt::Debug + Sized {
    type ExitValue: Default;

    fn file_name() -> &'static str;

    fn load<C, T>(service_group: &ServiceGroup, concrete_path: C, template_path: T) -> Option<Self>
        where C: AsRef<Path>,
              T: AsRef<Path>
    {
        let concrete = concrete_path.as_ref().join(Self::file_name());
        let template = template_path.as_ref().join(Self::file_name());
        match std::fs::metadata(&template) {
            Ok(_) => {
                match Self::new(concrete, template) {
                    Ok(hook) => Some(hook),
                    Err(err) => {
                        outputln!(preamble service_group, "Failed to load hook: {}", err);
                        None
                    }
                }
            }
            Err(_) => {
                debug!("{} not found at {}, not loading",
                       Self::file_name(),
                       template.display());
                None
            }
        }
    }

    fn new<C, T>(concrete_path: C, template_path: T) -> Result<Self>
        where C: Into<PathBuf>,
              T: AsRef<Path>;

    /// Compile a hook into it's destination service directory.
    fn compile(&self, cfg: &ServiceConfig) -> Result<()> {
        let toml = try!(cfg.to_toml());
        let svc_data = util::convert::toml_to_json(toml);
        let data = try!(self.template().render("hook", &svc_data));
        let mut file = try!(std::fs::File::create(self.path()));
        try!(file.write_all(data.as_bytes()));
        try!(hcore::util::perm::set_owner(self.path(), &cfg.pkg.svc_user, &cfg.pkg.svc_group));
        try!(hcore::util::perm::set_permissions(self.path(), HOOK_PERMISSIONS));
        debug!("{} compiled to {}",
               Self::file_name(),
               self.path().display());
        Ok(())
    }

    /// Run a compiled hook.
    fn run(&self, service_group: &ServiceGroup, cfg: &RuntimeConfig) -> Self::ExitValue {
        let mut child = match util::create_command(self.path(), &cfg.svc_user, &cfg.svc_group)
            .spawn() {
            Ok(child) => child,
            Err(err) => {
                outputln!(preamble service_group,
                    "Hook failed to run, {}, {}", Self::file_name(), err);
                return Self::ExitValue::default();
            }
        };
        stream_output::<Self>(service_group, &mut child);
        match child.wait() {
            Ok(status) => self.handle_exit(service_group, &status),
            Err(err) => {
                outputln!(preamble service_group,
                    "Hook failed to run, {}, {}", Self::file_name(), err);
                Self::ExitValue::default()
            }
        }
    }

    fn handle_exit(&self, group: &ServiceGroup, status: &ExitStatus) -> Self::ExitValue;

    fn path(&self) -> &Path;

    fn template(&self) -> &Template;
}

#[derive(Debug, Serialize)]
pub struct FileUpdatedHook(RenderPair);

impl Hook for FileUpdatedHook {
    type ExitValue = bool;

    fn file_name() -> &'static str {
        "file_updated"
    }

    fn new<C, T>(concrete_path: C, template_path: T) -> Result<Self>
        where C: Into<PathBuf>,
              T: AsRef<Path>
    {
        let pair = RenderPair::new(concrete_path, template_path)?;
        Ok(FileUpdatedHook(pair))
    }

    fn handle_exit(&self, _: &ServiceGroup, status: &ExitStatus) -> Self::ExitValue {
        status.success()
    }

    fn path(&self) -> &Path {
        &self.0.path
    }

    fn template(&self) -> &Template {
        &self.0.template
    }
}

#[derive(Debug, Serialize)]
pub struct HealthCheckHook(RenderPair);

impl Hook for HealthCheckHook {
    type ExitValue = health::HealthCheck;

    fn file_name() -> &'static str {
        "health_check"
    }

    fn new<C, T>(concrete_path: C, template_path: T) -> Result<Self>
        where C: Into<PathBuf>,
              T: AsRef<Path>
    {
        let pair = RenderPair::new(concrete_path, template_path)?;
        Ok(HealthCheckHook(pair))
    }

    fn handle_exit(&self, service_group: &ServiceGroup, status: &ExitStatus) -> Self::ExitValue {
        match status.code() {
            Some(0) => health::HealthCheck::Ok,
            Some(1) => health::HealthCheck::Warning,
            Some(2) => health::HealthCheck::Critical,
            Some(3) => health::HealthCheck::Unknown,
            Some(code) => {
                outputln!(preamble service_group,
                    "Health check exited with an unknown status code, {}", code);
                health::HealthCheck::default()
            }
            None => {
                outputln!(preamble service_group,
                    "{} exited without a status code", Self::file_name());
                health::HealthCheck::default()
            }
        }
    }

    fn path(&self) -> &Path {
        &self.0.path
    }

    fn template(&self) -> &Template {
        &self.0.template
    }
}

#[derive(Debug, Serialize)]
pub struct InitHook(RenderPair);

impl Hook for InitHook {
    type ExitValue = ExitCode;

    fn file_name() -> &'static str {
        "init"
    }

    fn new<C, T>(concrete_path: C, template_path: T) -> Result<Self>
        where C: Into<PathBuf>,
              T: AsRef<Path>
    {
        let pair = RenderPair::new(concrete_path, template_path)?;
        Ok(InitHook(pair))
    }

    fn handle_exit(&self, service_group: &ServiceGroup, status: &ExitStatus) -> Self::ExitValue {
        match status.code() {
            Some(code) => ExitCode(code),
            None => {
                outputln!(preamble service_group,
                    "{} exited without a status code", Self::file_name());
                ExitCode::default()
            }
        }
    }

    fn path(&self) -> &Path {
        &self.0.path
    }

    fn template(&self) -> &Template {
        &self.0.template
    }
}

#[derive(Debug, Serialize)]
pub struct ReconfigureHook(RenderPair);

impl Hook for ReconfigureHook {
    type ExitValue = ExitCode;

    fn file_name() -> &'static str {
        "reconfigure"
    }

    fn new<C, T>(concrete_path: C, template_path: T) -> Result<Self>
        where C: Into<PathBuf>,
              T: AsRef<Path>
    {
        let pair = RenderPair::new(concrete_path, template_path)?;
        Ok(ReconfigureHook(pair))
    }

    fn handle_exit(&self, service_group: &ServiceGroup, status: &ExitStatus) -> Self::ExitValue {
        match status.code() {
            Some(code) => ExitCode(code),
            None => {
                outputln!(preamble service_group,
                    "{} exited without a status code", Self::file_name());
                ExitCode::default()
            }
        }
    }

    fn path(&self) -> &Path {
        &self.0.path
    }

    fn template(&self) -> &Template {
        &self.0.template
    }
}

#[derive(Debug, Serialize)]
pub struct RunHook(RenderPair);

impl Hook for RunHook {
    type ExitValue = ExitCode;

    fn file_name() -> &'static str {
        "run"
    }

    fn new<C, T>(concrete_path: C, template_path: T) -> Result<Self>
        where C: Into<PathBuf>,
              T: AsRef<Path>
    {
        let pair = RenderPair::new(concrete_path, template_path)?;
        Ok(RunHook(pair))
    }

    fn handle_exit(&self, service_group: &ServiceGroup, status: &ExitStatus) -> Self::ExitValue {
        match status.code() {
            Some(code) => ExitCode(code),
            None => {
                outputln!(preamble service_group,
                    "{} exited without a status code", Self::file_name());
                ExitCode::default()
            }
        }
    }

    fn path(&self) -> &Path {
        &self.0.path
    }

    fn template(&self) -> &Template {
        &self.0.template
    }
}

#[derive(Debug, Serialize)]
pub struct SmokeTestHook(RenderPair);

impl Hook for SmokeTestHook {
    type ExitValue = health::SmokeCheck;

    fn file_name() -> &'static str {
        "smoke_test"
    }

    fn new<C, T>(concrete_path: C, template_path: T) -> Result<Self>
        where C: Into<PathBuf>,
              T: AsRef<Path>
    {
        let pair = RenderPair::new(concrete_path, template_path)?;
        Ok(SmokeTestHook(pair))
    }

    fn handle_exit(&self, service_group: &ServiceGroup, status: &ExitStatus) -> Self::ExitValue {
        match status.code() {
            Some(0) => health::SmokeCheck::Ok,
            Some(code) => health::SmokeCheck::Failed(code),
            None => {
                outputln!(preamble service_group,
                    "{} exited without a status code", Self::file_name());
                health::SmokeCheck::Failed(-1)
            }
        }
    }

    fn path(&self) -> &Path {
        &self.0.path
    }

    fn template(&self) -> &Template {
        &self.0.template
    }
}

#[derive(Debug, Default, Serialize)]
pub struct HookTable {
    pub health_check: Option<HealthCheckHook>,
    pub init: Option<InitHook>,
    pub file_updated: Option<FileUpdatedHook>,
    pub reconfigure: Option<ReconfigureHook>,
    pub run: Option<RunHook>,
    pub smoke_test: Option<SmokeTestHook>,
    cfg_incarnation: u64,
}

impl HookTable {
    /// Compile all loaded hooks from the table into their destination service directory.
    pub fn compile(&mut self, service_group: &ServiceGroup, config: &ServiceConfig) {
        if self.cfg_incarnation != 0 && config.incarnation <= self.cfg_incarnation {
            debug!("{}, Hooks already compiled with the latest configuration incarnation, \
                    skipping",
                   service_group);
            return;
        }
        self.cfg_incarnation = config.incarnation;
        if let Some(ref hook) = self.file_updated {
            self.compile_one(hook, service_group, config);
        }
        if let Some(ref hook) = self.health_check {
            self.compile_one(hook, service_group, config);
        }
        if let Some(ref hook) = self.init {
            self.compile_one(hook, service_group, config);
        }
        if let Some(ref hook) = self.reconfigure {
            self.compile_one(hook, service_group, config);
        }
        if let Some(ref hook) = self.run {
            self.compile_one(hook, service_group, config);
        }
        if let Some(ref hook) = self.smoke_test {
            self.compile_one(hook, service_group, config);
        }
        debug!("{}, Hooks compiled", service_group);
    }

    /// Read all available hook templates from the table's package directory into the table.
    pub fn load_hooks<T, U>(mut self, service_group: &ServiceGroup, hooks: T, templates: U) -> Self
        where T: AsRef<Path>,
              U: AsRef<Path>
    {
        if let Some(meta) = std::fs::metadata(templates.as_ref()).ok() {
            if meta.is_dir() {
                self.file_updated = FileUpdatedHook::load(service_group, &hooks, &templates);
                self.health_check = HealthCheckHook::load(service_group, &hooks, &templates);
                self.init = InitHook::load(service_group, &hooks, &templates);
                self.reconfigure = ReconfigureHook::load(service_group, &hooks, &templates);
                self.run = RunHook::load(service_group, &hooks, &templates);
                self.smoke_test = SmokeTestHook::load(service_group, &hooks, &templates);
            }
        }
        debug!("{}, Hooks loaded, destination={}, templates={}",
               service_group,
               hooks.as_ref().display(),
               templates.as_ref().display());
        self
    }

    fn compile_one<H>(&self, hook: &H, service_group: &ServiceGroup, config: &ServiceConfig)
        where H: Hook
    {
        hook.compile(config).unwrap_or_else(|e| {
            outputln!(preamble service_group,
                "Failed to compile {} hook: {}", H::file_name(), e);
        });
    }
}

struct RenderPair {
    path: PathBuf,
    template: Template,
}

impl RenderPair {
    pub fn new<C, T>(concrete_path: C, template_path: T) -> Result<Self>
        where C: Into<PathBuf>,
              T: AsRef<Path>
    {
        let mut template = Template::new();
        template.register_template_file("hook", template_path.as_ref())?;
        Ok(RenderPair {
            path: concrete_path.into(),
            template: template,
        })
    }
}

impl fmt::Debug for RenderPair {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "path: {}", self.path.display())
    }
}

impl Serialize for RenderPair {
    fn serialize<S>(&self, serializer: &mut S) -> result::Result<(), S::Error>
        where S: Serializer
    {
        serializer.serialize_str(&self.path.as_os_str().to_string_lossy().into_owned())
    }
}

fn stream_output<H: Hook>(service_group: &ServiceGroup, process: &mut Child) {
    let preamble_str = stream_preamble::<H>(service_group);
    if let Some(ref mut stdout) = process.stdout {
        for line in BufReader::new(stdout).lines() {
            if let Some(ref l) = line.ok() {
                outputln!(preamble preamble_str, l);
            }
        }
    }
    if let Some(ref mut stderr) = process.stderr {
        for line in BufReader::new(stderr).lines() {
            if let Some(ref l) = line.ok() {
                outputln!(preamble preamble_str, l);
            }
        }
    }
}

fn stream_preamble<H: Hook>(service_group: &ServiceGroup) -> String {
    format!("{} hook[{}]:", service_group, H::file_name())
}