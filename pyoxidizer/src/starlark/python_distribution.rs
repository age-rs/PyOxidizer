// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use {
    super::{
        env::{get_context, EnvironmentContext},
        python_executable::PythonExecutable,
        python_interpreter_config::PythonInterpreterConfigValue,
        python_packaging_policy::PythonPackagingPolicyValue,
        python_resource::{
            add_context_for_value, python_resource_to_value, PythonExtensionModuleValue,
            PythonModuleSourceValue, PythonPackageResourceValue,
        },
        util::{optional_str_arg, optional_type_arg, required_bool_arg, required_str_arg},
    },
    crate::py_packaging::{
        distribution::BinaryLibpythonLinkMode,
        distribution::{
            default_distribution_location, is_stdlib_test_package, resolve_distribution,
            DistributionFlavor, PythonDistribution as PythonDistributionTrait,
            PythonDistributionLocation,
        },
    },
    anyhow::{anyhow, Result},
    itertools::Itertools,
    python_packaging::{
        bytecode::{CompileMode, PythonBytecodeCompiler},
        policy::PythonPackagingPolicy,
        resource::{BytecodeOptimizationLevel, PythonResource},
        resource_collection::PythonResourceAddCollectionContext,
    },
    starlark::{
        environment::TypeValues,
        eval::call_stack::CallStack,
        values::{
            error::{RuntimeError, ValueError, INCORRECT_PARAMETER_TYPE_ERROR_CODE},
            none::NoneType,
            {Mutable, TypedValue, Value, ValueResult},
        },
        {
            starlark_fun, starlark_module, starlark_parse_param_type, starlark_signature,
            starlark_signature_extraction, starlark_signatures,
        },
    },
    std::{
        convert::TryFrom,
        path::{Path, PathBuf},
        sync::Arc,
    },
};

pub struct PythonDistribution {
    flavor: DistributionFlavor,
    pub source: PythonDistributionLocation,

    dest_dir: PathBuf,

    pub distribution: Option<Arc<Box<dyn PythonDistributionTrait>>>,

    compiler: Option<Box<dyn PythonBytecodeCompiler>>,
}

impl PythonDistribution {
    fn from_location(
        flavor: DistributionFlavor,
        location: PythonDistributionLocation,
        dest_dir: &Path,
    ) -> PythonDistribution {
        PythonDistribution {
            flavor,
            source: location,
            dest_dir: dest_dir.to_path_buf(),
            distribution: None,
            compiler: None,
        }
    }

    pub fn ensure_distribution_resolved(&mut self, logger: &slog::Logger) -> Result<()> {
        if self.distribution.is_some() {
            return Ok(());
        }

        let dist = resolve_distribution(logger, &self.flavor, &self.source, &self.dest_dir)?;
        //warn!(logger, "distribution info: {:#?}", dist.as_minimal_info());

        self.distribution = Some(Arc::new(dist));

        Ok(())
    }

    /// Compile bytecode using this distribution.
    ///
    /// A bytecode compiler will be lazily instantiated and preserved for the
    /// lifetime of the instance. So calling multiple times does not pay a
    /// recurring performance penalty for instantiating the bytecode compiler.
    pub fn compile_bytecode(
        &mut self,
        logger: &slog::Logger,
        source: &[u8],
        filename: &str,
        optimize: BytecodeOptimizationLevel,
        output_mode: CompileMode,
    ) -> Result<Vec<u8>> {
        self.ensure_distribution_resolved(logger)?;

        if let Some(dist) = &self.distribution {
            if self.compiler.is_none() {
                self.compiler = Some(dist.create_bytecode_compiler()?);
            }
        }

        if let Some(compiler) = &mut self.compiler {
            compiler.compile(source, filename, optimize, output_mode)
        } else {
            Err(anyhow!("bytecode compiler should exist"))
        }
    }
}

impl TypedValue for PythonDistribution {
    type Holder = Mutable<PythonDistribution>;
    const TYPE: &'static str = "PythonDistribution";

    fn values_for_descendant_check_and_freeze(&self) -> Box<dyn Iterator<Item = Value>> {
        Box::new(std::iter::empty())
    }

    fn to_str(&self) -> String {
        format!("PythonDistribution<{:#?}>", self.source)
    }
}

// Starlark functions.
impl PythonDistribution {
    /// default_python_distribution(flavor, build_target=None, python_version=None)
    fn default_python_distribution(
        type_values: &TypeValues,
        flavor: &Value,
        build_target: &Value,
        python_version: &Value,
    ) -> ValueResult {
        let flavor = required_str_arg("flavor", flavor)?;
        let build_target = optional_str_arg("build_target", build_target)?;
        let python_version = optional_str_arg("python_version", &python_version)?;

        let raw_context = get_context(type_values)?;
        let context = raw_context
            .downcast_ref::<EnvironmentContext>()
            .ok_or(ValueError::IncorrectParameterType)?;

        let build_target = match build_target {
            Some(t) => t,
            None => context.build_target_triple.clone(),
        };

        let flavor = DistributionFlavor::try_from(flavor.as_str()).map_err(|e| {
            ValueError::from(RuntimeError {
                code: "PYOXIDIZER_BUILD",
                message: e,
                label: "default_python_distribution()".to_string(),
            })
        })?;

        let python_version_str = match &python_version {
            Some(x) => Some(x.as_str()),
            None => None,
        };

        let location = default_distribution_location(&flavor, &build_target, python_version_str)
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "PYOXIDIZER_BUILD",
                    message: e.to_string(),
                    label: "default_python_distribution()".to_string(),
                })
            })?;

        let raw_context = get_context(type_values)?;
        let context = raw_context
            .downcast_ref::<EnvironmentContext>()
            .ok_or(ValueError::IncorrectParameterType)?;

        Ok(Value::new(PythonDistribution::from_location(
            flavor,
            location,
            &context.python_distributions_path,
        )))
    }

    /// PythonDistribution()
    fn from_args(
        type_values: &TypeValues,
        sha256: &Value,
        local_path: &Value,
        url: &Value,
        flavor: &Value,
    ) -> ValueResult {
        required_str_arg("sha256", sha256)?;
        optional_str_arg("local_path", local_path)?;
        optional_str_arg("url", url)?;
        let flavor = required_str_arg("flavor", flavor)?;

        if local_path.get_type() != "NoneType" && url.get_type() != "NoneType" {
            return Err(ValueError::from(RuntimeError {
                code: INCORRECT_PARAMETER_TYPE_ERROR_CODE,
                message: "cannot define both local_path and url".to_string(),
                label: "cannot define both local_path and url".to_string(),
            }));
        }

        let distribution = if local_path.get_type() != "NoneType" {
            PythonDistributionLocation::Local {
                local_path: local_path.to_string(),
                sha256: sha256.to_string(),
            }
        } else {
            PythonDistributionLocation::Url {
                url: url.to_string(),
                sha256: sha256.to_string(),
            }
        };

        let flavor = match flavor.as_ref() {
            "standalone" => DistributionFlavor::Standalone,
            v => {
                return Err(ValueError::from(RuntimeError {
                    code: "PYOXIDIZER_BUILD",
                    message: format!("invalid distribution flavor {}", v),
                    label: "PythonDistribution()".to_string(),
                }))
            }
        };

        let raw_context = get_context(type_values)?;
        let context = raw_context
            .downcast_ref::<EnvironmentContext>()
            .ok_or(ValueError::IncorrectParameterType)?;

        Ok(Value::new(PythonDistribution::from_location(
            flavor,
            distribution,
            &context.python_distributions_path,
        )))
    }

    /// PythonDistribution.make_python_packaging_policy()
    fn make_python_packaging_policy_starlark(&mut self, type_values: &TypeValues) -> ValueResult {
        let raw_context = get_context(type_values)?;
        let context = raw_context
            .downcast_ref::<EnvironmentContext>()
            .ok_or(ValueError::IncorrectParameterType)?;

        self.ensure_distribution_resolved(&context.logger)
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "PYOXIDIZER_BUILD",
                    message: e.to_string(),
                    label: "resolve_distribution()".to_string(),
                })
            })?;
        let dist = self.distribution.as_ref().unwrap().clone();

        let policy = dist.create_packaging_policy().map_err(|e| {
            ValueError::from(RuntimeError {
                code: "PYOXIDIZER_BUILD",
                message: e.to_string(),
                label: "make_python_packaging_policy()".to_string(),
            })
        })?;

        Ok(Value::new(PythonPackagingPolicyValue::new(policy)))
    }

    /// PythonDistribution.make_python_interpreter_config()
    fn make_python_interpreter_config_starlark(&mut self, type_values: &TypeValues) -> ValueResult {
        let raw_context = get_context(type_values)?;
        let context = raw_context
            .downcast_ref::<EnvironmentContext>()
            .ok_or(ValueError::IncorrectParameterType)?;

        self.ensure_distribution_resolved(&context.logger)
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "PYOXIDIZER_BUILD",
                    message: e.to_string(),
                    label: "resolve_distribution()".to_string(),
                })
            })?;
        let dist = self.distribution.as_ref().unwrap().clone();

        let config = dist.create_python_interpreter_config().map_err(|e| {
            ValueError::from(RuntimeError {
                code: "PYOXIDIZER_BUILD",
                message: e.to_string(),
                label: "make_python_packaging_policy()".to_string(),
            })
        })?;

        Ok(Value::new(PythonInterpreterConfigValue::new(config)))
    }

    /// PythonDistribution.to_python_executable(
    ///     name,
    ///     packaging_policy=None,
    ///     config=None,
    /// )
    #[allow(
        clippy::ptr_arg,
        clippy::too_many_arguments,
        clippy::clippy::wrong_self_convention
    )]
    fn to_python_executable_starlark(
        &mut self,
        type_values: &TypeValues,
        call_stack: &mut CallStack,
        name: &Value,
        packaging_policy: &Value,
        config: &Value,
    ) -> ValueResult {
        let name = required_str_arg("name", &name)?;
        optional_type_arg(
            "packaging_policy",
            "PythonPackagingPolicy",
            &packaging_policy,
        )?;
        optional_type_arg("config", "PythonInterpreterConfig", &config)?;

        let raw_context = get_context(type_values)?;
        let context = raw_context
            .downcast_ref::<EnvironmentContext>()
            .ok_or(ValueError::IncorrectParameterType)?;

        self.ensure_distribution_resolved(&context.logger)
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "PYOXIDIZER_BUILD",
                    message: e.to_string(),
                    label: "resolve_distribution()".to_string(),
                })
            })?;
        let dist = self.distribution.as_ref().unwrap().clone();

        let policy = if packaging_policy.get_type() == "NoneType" {
            Ok(PythonPackagingPolicyValue::new(
                dist.create_packaging_policy().map_err(|e| {
                    ValueError::from(RuntimeError {
                        code: "PYOXIDIZER_BUILD",
                        message: e.to_string(),
                        label: "resolve_distribution()".to_string(),
                    })
                })?,
            ))
        } else {
            match packaging_policy.downcast_ref::<PythonPackagingPolicyValue>() {
                Some(policy) => Ok(policy.clone()),
                None => Err(ValueError::IncorrectParameterType),
            }
        }?;

        let config = if config.get_type() == "NoneType" {
            Ok(PythonInterpreterConfigValue::new(
                dist.create_python_interpreter_config().map_err(|e| {
                    ValueError::from(RuntimeError {
                        code: "PYOXIDIZER_BUILD",
                        message: e.to_string(),
                        label: "resolve_distribution()".to_string(),
                    })
                })?,
            ))
        } else {
            match config.downcast_ref::<PythonInterpreterConfigValue>() {
                Some(c) => Ok(c.clone()),
                None => Err(ValueError::IncorrectParameterType),
            }
        }?;

        let host_distribution = if dist
            .compatible_host_triples()
            .contains(&context.build_host_triple)
        {
            Some(dist.clone())
        } else {
            let flavor = DistributionFlavor::Standalone;
            let location = default_distribution_location(
                &flavor,
                &context.build_host_triple,
                Some(dist.python_major_minor_version().as_str()),
            )
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "PYOXIDIZER_BUILD",
                    message: format!("unable to find host Python distribution: {}", e),
                    label: "to_python_executable()".to_string(),
                })
            })?;

            Some(Arc::new(
                resolve_distribution(
                    &context.logger,
                    &flavor,
                    &location,
                    &context.python_distributions_path,
                )
                .map_err(|e| {
                    ValueError::from(RuntimeError {
                        code: "PYOXIDIZER_BUILD",
                        message: format!("unable to resolve host Python distribution: {}", e),
                        label: "to_python_executable".to_string(),
                    })
                })?,
            ))
        };

        let mut builder = dist
            .as_python_executable_builder(
                &context.logger,
                &context.build_host_triple,
                &context.build_target_triple,
                &name,
                // TODO make configurable
                BinaryLibpythonLinkMode::Default,
                &policy.inner,
                &config.inner,
                host_distribution,
            )
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "PYOXIDIZER_BUILD",
                    message: e.to_string(),
                    label: "to_python_executable()".to_string(),
                })
            })?;

        let callback = Box::new(
            |_policy: &PythonPackagingPolicy,
             resource: &PythonResource,
             add_context: &mut PythonResourceAddCollectionContext|
             -> Result<()> {
                // Callback is declared Fn, so we can't take a mutable reference.
                // A copy should be fine.
                let mut cs = call_stack.clone();

                // There is a PythonPackagingPolicy passed into this callback
                // and one passed into the outer function as a &Value. The
                // former is derived from the latter. And the latter has Starlark
                // callbacks registered on it.
                //
                // When we call python_resource_to_value(), the Starlark
                // callbacks are automatically called.

                let value = python_resource_to_value(&type_values, &mut cs, resource, &policy)
                    .map_err(|e| anyhow!("error converting PythonResource to Value: {:?}", e))?;

                let new_add_context = add_context_for_value(&value, "to_python_executable")
                    .map_err(|e| anyhow!("error obtaining add context from Value: {:?}", e))?
                    .expect("add context should have been populated as part of Value conversion");

                add_context.replace(&new_add_context);

                Ok(())
            },
        );

        builder
            .add_distribution_resources(Some(callback))
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "PYOXIDIZER_BUILD",
                    message: e.to_string(),
                    label: "to_python_executable()".to_string(),
                })
            })?;

        Ok(Value::new(PythonExecutable::new(builder, policy)))
    }

    /// PythonDistribution.extension_modules()
    pub fn extension_modules(&mut self, type_values: &TypeValues) -> ValueResult {
        let raw_context = get_context(type_values)?;
        let context = raw_context
            .downcast_ref::<EnvironmentContext>()
            .ok_or(ValueError::IncorrectParameterType)?;

        self.ensure_distribution_resolved(&context.logger)
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "PYOXIDIZER_BUILD",
                    message: e.to_string(),
                    label: "resolve_distribution()".to_string(),
                })
            })?;

        Ok(Value::from(
            self.distribution
                .as_ref()
                .unwrap()
                .iter_extension_modules()
                .map(|em| Value::new(PythonExtensionModuleValue::new(em.clone())))
                .collect_vec(),
        ))
    }

    /// PythonDistribution.package_resources(include_test=false)
    pub fn package_resources(
        &mut self,
        type_values: &TypeValues,
        include_test: &Value,
    ) -> ValueResult {
        let include_test = required_bool_arg("include_test", &include_test)?;

        let raw_context = get_context(type_values)?;
        let context = raw_context
            .downcast_ref::<EnvironmentContext>()
            .ok_or(ValueError::IncorrectParameterType)?;

        self.ensure_distribution_resolved(&context.logger)
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "PYOXIDIZER_BUILD",
                    message: e.to_string(),
                    label: "resolve_distribution()".to_string(),
                })
            })?;

        let resources = self
            .distribution
            .as_ref()
            .unwrap()
            .resource_datas()
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "PYTHON_DISTRIBUTION",
                    message: e.to_string(),
                    label: e.to_string(),
                })
            })?;

        Ok(Value::from(
            resources
                .iter()
                .filter_map(|data| {
                    if !include_test && is_stdlib_test_package(&data.leaf_package) {
                        None
                    } else {
                        Some(Value::new(PythonPackageResourceValue::new(data.clone())))
                    }
                })
                .collect_vec(),
        ))
    }

    /// PythonDistribution.source_modules()
    pub fn source_modules(&mut self, type_values: &TypeValues) -> ValueResult {
        let raw_context = get_context(type_values)?;
        let context = raw_context
            .downcast_ref::<EnvironmentContext>()
            .ok_or(ValueError::IncorrectParameterType)?;

        self.ensure_distribution_resolved(&context.logger)
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "PYOXIDIZER_BUILD",
                    message: e.to_string(),
                    label: "resolve_distribution()".to_string(),
                })
            })?;

        let modules = self
            .distribution
            .as_ref()
            .unwrap()
            .source_modules()
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "PYTHON_DISTRIBUTION",
                    message: e.to_string(),
                    label: e.to_string(),
                })
            })?;

        Ok(Value::from(
            modules
                .iter()
                .map(|module| Value::new(PythonModuleSourceValue::new(module.clone())))
                .collect_vec(),
        ))
    }
}

starlark_module! { python_distribution_module =>
    #[allow(non_snake_case, clippy::ptr_arg)]
    PythonDistribution(env env, sha256, local_path=NoneType::None, url=NoneType::None, flavor="standalone") {
        PythonDistribution::from_args(&env, &sha256, &local_path, &url, &flavor)
    }

    PythonDistribution.make_python_packaging_policy(env env, this) {
        match this.clone().downcast_mut::<PythonDistribution>()? {
            Some(mut dist) => dist.make_python_packaging_policy_starlark(&env),
            None => Err(ValueError::IncorrectParameterType),
        }
    }

    PythonDistribution.make_python_interpreter_config(env env, this) {
        match this.clone().downcast_mut::<PythonDistribution>()? {
            Some(mut dist) => dist.make_python_interpreter_config_starlark(&env),
            None => Err(ValueError::IncorrectParameterType),
        }
    }

    #[allow(clippy::ptr_arg)]
    PythonDistribution.extension_modules(env env, this) {
        match this.clone().downcast_mut::<PythonDistribution>()? {
            Some(mut dist) => dist.extension_modules(&env),
            None => Err(ValueError::IncorrectParameterType),
        }
    }

    #[allow(clippy::ptr_arg)]
    PythonDistribution.source_modules(env env, this) {
        match this.clone().downcast_mut::<PythonDistribution>()? {
            Some(mut dist) => dist.source_modules(&env),
            None => Err(ValueError::IncorrectParameterType),
        }
    }

    #[allow(clippy::ptr_arg)]
    PythonDistribution.package_resources(env env, this, include_test=false) {
        match this.clone().downcast_mut::<PythonDistribution>()? {
            Some(mut dist) => dist.package_resources(&env, &include_test),
            None => Err(ValueError::IncorrectParameterType),
        }
    }

    #[allow(non_snake_case, clippy::ptr_arg)]
    PythonDistribution.to_python_executable(
        env env,
        call_stack cs,
        this,
        name,
        packaging_policy=NoneType::None,
        config=NoneType::None
    ) {
        match this.clone().downcast_mut::<PythonDistribution>()? {
            Some(mut dist) =>dist.to_python_executable_starlark(
                &env,
                cs,
                &name,
                &packaging_policy,
                &config,
            ),
            None => Err(ValueError::IncorrectParameterType),
        }
    }

    #[allow(clippy::ptr_arg)]
    default_python_distribution(
        env env,
        flavor="standalone",
        build_target=NoneType::None,
        python_version=NoneType::None
    ) {
        PythonDistribution::default_python_distribution(&env, &flavor, &build_target, &python_version)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::super::testutil::*, super::*, crate::py_packaging::distribution::DistributionFlavor,
        crate::python_distributions::PYTHON_DISTRIBUTIONS,
    };

    #[test]
    fn test_default_python_distribution() {
        let dist = starlark_ok("default_python_distribution()");
        assert_eq!(dist.get_type(), "PythonDistribution");

        let host_distribution = PYTHON_DISTRIBUTIONS
            .find_distribution(
                crate::project_building::HOST,
                &DistributionFlavor::Standalone,
                None,
            )
            .unwrap();

        let x = dist.downcast_ref::<PythonDistribution>().unwrap();
        assert_eq!(x.source, host_distribution.location)
    }

    #[test]
    fn test_default_python_distribution_bad_arg() {
        let err = starlark_nok("default_python_distribution(False)");
        assert_eq!(
            err.message,
            "function expects a string for flavor; got type bool"
        );
    }

    #[test]
    fn test_default_python_distribution_python_38() -> Result<()> {
        let mut env = StarlarkEnvironment::new()?;

        let dist = env.eval("default_python_distribution(python_version='3.8')")?;
        assert_eq!(dist.get_type(), "PythonDistribution");

        let wanted = PYTHON_DISTRIBUTIONS
            .find_distribution(
                crate::project_building::HOST,
                &DistributionFlavor::Standalone,
                Some("3.8"),
            )
            .unwrap();

        let x = dist.downcast_ref::<PythonDistribution>().unwrap();
        assert_eq!(x.source, wanted.location);

        Ok(())
    }

    #[test]
    fn test_default_python_distribution_python_39() -> Result<()> {
        let mut env = StarlarkEnvironment::new()?;

        let dist = env.eval("default_python_distribution(python_version='3.9')")?;
        assert_eq!(dist.get_type(), "PythonDistribution");

        let wanted = PYTHON_DISTRIBUTIONS
            .find_distribution(
                crate::project_building::HOST,
                &DistributionFlavor::Standalone,
                Some("3.9"),
            )
            .unwrap();

        let x = dist.downcast_ref::<PythonDistribution>().unwrap();
        assert_eq!(x.source, wanted.location);

        Ok(())
    }

    #[test]
    #[cfg(windows)]
    fn test_default_python_distribution_dynamic_windows() {
        let dist = starlark_ok("default_python_distribution(flavor='standalone_dynamic')");
        assert_eq!(dist.get_type(), "PythonDistribution");

        let host_distribution = PYTHON_DISTRIBUTIONS
            .find_distribution(
                crate::project_building::HOST,
                &DistributionFlavor::StandaloneDynamic,
                None,
            )
            .unwrap();

        let x = dist.downcast_ref::<PythonDistribution>().unwrap();
        assert_eq!(x.source, host_distribution.location)
    }

    #[test]
    fn test_python_distribution_no_args() {
        let err = starlark_nok("PythonDistribution()");
        assert!(err.message.starts_with("Missing parameter sha256"));
    }

    #[test]
    fn test_python_distribution_multiple_args() {
        let err = starlark_nok(
            "PythonDistribution('sha256', url='url_value', local_path='local_path_value')",
        );
        assert_eq!(err.message, "cannot define both local_path and url");
    }

    #[test]
    fn test_python_distribution_url() {
        let dist = starlark_ok("PythonDistribution('sha256', url='some_url')");
        let wanted = PythonDistributionLocation::Url {
            url: "some_url".to_string(),
            sha256: "sha256".to_string(),
        };

        let x = dist.downcast_ref::<PythonDistribution>().unwrap();
        assert_eq!(x.source, wanted);
        assert_eq!(x.flavor, DistributionFlavor::Standalone);
    }

    #[test]
    fn test_python_distribution_local_path() {
        let dist = starlark_ok("PythonDistribution('sha256', local_path='some_path')");
        let wanted = PythonDistributionLocation::Local {
            local_path: "some_path".to_string(),
            sha256: "sha256".to_string(),
        };

        let x = dist.downcast_ref::<PythonDistribution>().unwrap();
        assert_eq!(x.source, wanted);
        assert_eq!(x.flavor, DistributionFlavor::Standalone);
    }

    #[test]
    fn test_make_python_packaging_policy() {
        let policy = starlark_ok("default_python_distribution().make_python_packaging_policy()");
        assert_eq!(policy.get_type(), "PythonPackagingPolicy");
    }

    #[test]
    fn test_make_python_interpreter_config() {
        let config = starlark_ok("default_python_distribution().make_python_interpreter_config()");
        assert_eq!(config.get_type(), "PythonInterpreterConfig");
    }

    #[test]
    fn test_source_modules() {
        let mods = starlark_ok("default_python_distribution().source_modules()");
        assert_eq!(mods.get_type(), "list");

        for m in mods.iter().unwrap().iter() {
            assert_eq!(m.get_type(), PythonModuleSourceValue::TYPE);
            assert!(m.get_attr("is_stdlib").unwrap().to_bool());
        }
    }

    #[test]
    fn test_package_resources() {
        let data_default = starlark_ok("default_python_distribution().package_resources()");
        let data_tests =
            starlark_ok("default_python_distribution().package_resources(include_test=True)");

        let default_length = data_default.length().unwrap();
        let data_length = data_tests.length().unwrap();

        assert!(default_length < data_length);

        for r in data_tests.iter().unwrap().iter() {
            assert_eq!(r.get_type(), PythonPackageResourceValue::TYPE);
            assert!(r.get_attr("is_stdlib").unwrap().to_bool());
        }
    }

    #[test]
    fn test_extension_modules() {
        let mods = starlark_ok("default_python_distribution().extension_modules()");
        assert_eq!(mods.get_type(), "list");

        for m in mods.iter().unwrap().iter() {
            assert_eq!(m.get_type(), PythonExtensionModuleValue::TYPE);
            assert!(m.get_attr("is_stdlib").unwrap().to_bool());
        }
    }
}
