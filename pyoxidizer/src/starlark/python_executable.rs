// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use {
    super::{
        env::{get_context, EnvironmentContext},
        python_embedded_resources::PythonEmbeddedResources,
        python_packaging_policy::PythonPackagingPolicyValue,
        python_resource::{
            is_resource_starlark_compatible, python_resource_to_value, PythonExtensionModuleValue,
            PythonModuleSourceValue, PythonPackageDistributionResourceValue,
            PythonPackageResourceValue, ResourceCollectionContext,
        },
        target::{BuildContext, BuildTarget, ResolvedTarget, RunMode},
        util::{
            optional_dict_arg, optional_list_arg, required_bool_arg, required_list_arg,
            required_str_arg,
        },
    },
    crate::{project_building::build_python_executable, py_packaging::binary::PythonBinaryBuilder},
    anyhow::{Context, Result},
    python_packaging::resource::{DataLocation, PythonModuleSource},
    slog::{info, warn},
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
        collections::HashMap,
        io::Write,
        ops::Deref,
        path::{Path, PathBuf},
    },
};

/// Represents a builder for a Python executable.
pub struct PythonExecutable {
    pub exe: Box<dyn PythonBinaryBuilder>,

    /// The Starlark Value for the Python packaging policy.
    // This is stored as a Vec because I couldn't figure out how to implement
    // values_for_descendant_check_and_freeze() without the borrow checker
    // complaining due to a temporary vec/array.
    policy: Vec<Value>,
}

impl PythonExecutable {
    pub fn new(exe: Box<dyn PythonBinaryBuilder>, policy: PythonPackagingPolicyValue) -> Self {
        Self {
            exe,
            policy: vec![Value::new(policy)],
        }
    }

    /// Obtains a copy of the `PythonPackagingPolicyValue` stored internally.
    pub fn python_packaging_policy(&self) -> PythonPackagingPolicyValue {
        self.policy[0]
            .downcast_ref::<PythonPackagingPolicyValue>()
            .unwrap()
            .clone()
    }
}

impl TypedValue for PythonExecutable {
    type Holder = Mutable<PythonExecutable>;
    const TYPE: &'static str = "PythonExecutable";

    fn values_for_descendant_check_and_freeze<'a>(
        &'a self,
    ) -> Box<dyn Iterator<Item = Value> + 'a> {
        Box::new(self.policy.iter().cloned())
    }
}

impl BuildTarget for PythonExecutable {
    fn build(&mut self, context: &BuildContext) -> Result<ResolvedTarget> {
        // Build an executable by writing out a temporary Rust project
        // and building it.
        let build = build_python_executable(
            &context.logger,
            &self.exe.name(),
            self.exe.deref(),
            &context.target_triple,
            &context.opt_level,
            context.release,
        )?;

        let dest_path = context.output_path.join(build.exe_name);
        warn!(
            &context.logger,
            "writing executable to {}",
            dest_path.display()
        );
        let mut fh = std::fs::File::create(&dest_path)
            .context(format!("creating {}", dest_path.display()))?;
        fh.write_all(&build.exe_data)
            .context(format!("writing {}", dest_path.display()))?;

        crate::app_packaging::resource::set_executable(&mut fh)
            .context("making binary executable")?;

        Ok(ResolvedTarget {
            run_mode: RunMode::Path { path: dest_path },
            output_path: context.output_path.clone(),
        })
    }
}

// Starlark functions.
impl PythonExecutable {
    /// PythonExecutable.make_python_module_source(name, source, is_package=false)
    pub fn starlark_make_python_module_source(
        &self,
        type_values: &TypeValues,
        call_stack: &mut CallStack,
        name: &Value,
        source: &Value,
        is_package: &Value,
    ) -> ValueResult {
        let name = required_str_arg("name", &name)?;
        let source = required_str_arg("source", &source)?;
        let is_package = required_bool_arg("is_package", &is_package)?;

        let module = PythonModuleSource {
            name,
            source: DataLocation::Memory(source.into_bytes()),
            is_package,
            cache_tag: self.exe.cache_tag().to_string(),
            is_stdlib: false,
            is_test: false,
        };

        let mut value = PythonModuleSourceValue::new(module);
        self.python_packaging_policy()
            .apply_to_resource(type_values, call_stack, &mut value)?;

        Ok(Value::new(value))
    }

    /// PythonExecutable.pip_download(args)
    pub fn starlark_pip_download(
        &self,
        type_values: &TypeValues,
        call_stack: &mut CallStack,
        args: &Value,
    ) -> ValueResult {
        required_list_arg("args", "string", &args)?;

        let args: Vec<String> = args.iter()?.iter().map(|x| x.to_string()).collect();

        let raw_context = get_context(type_values)?;
        let context = raw_context
            .downcast_ref::<EnvironmentContext>()
            .ok_or(ValueError::IncorrectParameterType)?;

        let resources = self
            .exe
            .pip_download(&context.logger, context.verbose, &args)
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "PIP_INSTALL_ERROR",
                    message: format!("error running pip install: {}", e),
                    label: "pip_install()".to_string(),
                })
            })?
            .iter()
            .filter(|r| is_resource_starlark_compatible(r))
            .map(|r| {
                python_resource_to_value(
                    type_values,
                    call_stack,
                    r,
                    &self.python_packaging_policy(),
                )
            })
            .collect::<Result<Vec<Value>, ValueError>>()?;

        Ok(Value::from(resources))
    }

    /// PythonExecutable.pip_install(args, extra_envs=None)
    pub fn starlark_pip_install(
        &self,
        type_values: &TypeValues,
        call_stack: &mut CallStack,
        args: &Value,
        extra_envs: &Value,
    ) -> ValueResult {
        required_list_arg("args", "string", &args)?;
        optional_dict_arg("extra_envs", "string", "string", &extra_envs)?;

        let args: Vec<String> = args.iter()?.iter().map(|x| x.to_string()).collect();

        let extra_envs = match extra_envs.get_type() {
            "dict" => extra_envs
                .iter()?
                .iter()
                .map(|key| {
                    let k = key.to_string();
                    let v = extra_envs.at(key).unwrap().to_string();
                    (k, v)
                })
                .collect(),
            "NoneType" => HashMap::new(),
            _ => panic!("should have validated type above"),
        };

        let raw_context = get_context(type_values)?;
        let context = raw_context
            .downcast_ref::<EnvironmentContext>()
            .ok_or(ValueError::IncorrectParameterType)?;

        let resources = self
            .exe
            .pip_install(&context.logger, context.verbose, &args, &extra_envs)
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "PIP_INSTALL_ERROR",
                    message: format!("error running pip install: {}", e),
                    label: "pip_install()".to_string(),
                })
            })?
            .iter()
            .filter(|r| is_resource_starlark_compatible(r))
            .map(|r| {
                python_resource_to_value(
                    type_values,
                    call_stack,
                    r,
                    &self.python_packaging_policy(),
                )
            })
            .collect::<Result<Vec<Value>, ValueError>>()?;

        Ok(Value::from(resources))
    }

    /// PythonExecutable.read_package_root(path, packages)
    pub fn starlark_read_package_root(
        &self,
        type_values: &TypeValues,
        call_stack: &mut CallStack,
        path: &Value,
        packages: &Value,
    ) -> ValueResult {
        let path = required_str_arg("path", &path)?;
        required_list_arg("packages", "string", &packages)?;

        let packages = packages
            .iter()?
            .iter()
            .map(|x| x.to_string())
            .collect::<Vec<String>>();

        let raw_context = get_context(type_values)?;
        let context = raw_context
            .downcast_ref::<EnvironmentContext>()
            .ok_or(ValueError::IncorrectParameterType)?;

        let resources = self
            .exe
            .read_package_root(&context.logger, Path::new(&path), &packages)
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "PACKAGE_ROOT_ERROR",
                    message: format!("could not find resources: {}", e),
                    label: "read_package_root()".to_string(),
                })
            })?
            .iter()
            .filter(|r| is_resource_starlark_compatible(r))
            .map(|r| {
                python_resource_to_value(
                    type_values,
                    call_stack,
                    r,
                    &self.python_packaging_policy(),
                )
            })
            .collect::<Result<Vec<Value>, ValueError>>()?;

        Ok(Value::from(resources))
    }

    /// PythonExecutable.read_virtualenv(path)
    pub fn starlark_read_virtualenv(
        &self,
        type_values: &TypeValues,
        call_stack: &mut CallStack,
        path: &Value,
    ) -> ValueResult {
        let path = required_str_arg("path", &path)?;

        let raw_context = get_context(type_values)?;
        let context = raw_context
            .downcast_ref::<EnvironmentContext>()
            .ok_or(ValueError::IncorrectParameterType)?;

        let resources = self
            .exe
            .read_virtualenv(&context.logger, &Path::new(&path))
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "VIRTUALENV_ERROR",
                    message: format!("could not find resources: {}", e),
                    label: "read_virtualenv()".to_string(),
                })
            })?
            .iter()
            .filter(|r| is_resource_starlark_compatible(r))
            .map(|r| {
                python_resource_to_value(
                    type_values,
                    call_stack,
                    r,
                    &self.python_packaging_policy(),
                )
            })
            .collect::<Result<Vec<Value>, ValueError>>()?;

        Ok(Value::from(resources))
    }

    /// PythonExecutable.setup_py_install(package_path, extra_envs=None, extra_global_arguments=None)
    pub fn starlark_setup_py_install(
        &self,
        type_values: &TypeValues,
        call_stack: &mut CallStack,
        package_path: &Value,
        extra_envs: &Value,
        extra_global_arguments: &Value,
    ) -> ValueResult {
        let package_path = required_str_arg("package_path", &package_path)?;
        optional_dict_arg("extra_envs", "string", "string", &extra_envs)?;
        optional_list_arg("extra_global_arguments", "string", &extra_global_arguments)?;

        let extra_envs = match extra_envs.get_type() {
            "dict" => extra_envs
                .iter()?
                .iter()
                .map(|key| {
                    let k = key.to_string();
                    let v = extra_envs.at(key).unwrap().to_string();
                    (k, v)
                })
                .collect(),
            "NoneType" => HashMap::new(),
            _ => panic!("should have validated type above"),
        };
        let extra_global_arguments = match extra_global_arguments.get_type() {
            "list" => extra_global_arguments
                .iter()?
                .iter()
                .map(|x| x.to_string())
                .collect(),
            "NoneType" => Vec::new(),
            _ => panic!("should have validated type above"),
        };

        let package_path = PathBuf::from(package_path);

        let raw_context = get_context(type_values)?;
        let context = raw_context
            .downcast_ref::<EnvironmentContext>()
            .ok_or(ValueError::IncorrectParameterType)?;

        let package_path = if package_path.is_absolute() {
            package_path
        } else {
            PathBuf::from(&context.cwd).join(package_path)
        };

        let resources = self
            .exe
            .setup_py_install(
                &context.logger,
                &package_path,
                context.verbose,
                &extra_envs,
                &extra_global_arguments,
            )
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "SETUP_PY_ERROR",
                    message: e.to_string(),
                    label: "setup_py_install()".to_string(),
                })
            })?
            .iter()
            .filter(|r| is_resource_starlark_compatible(r))
            .map(|r| {
                python_resource_to_value(
                    type_values,
                    call_stack,
                    r,
                    &self.python_packaging_policy(),
                )
            })
            .collect::<Result<Vec<Value>, ValueError>>()?;

        warn!(
            &context.logger,
            "collected {} resources from setup.py install",
            resources.len()
        );

        Ok(Value::from(resources))
    }

    pub fn add_python_module_source(
        &mut self,
        context: &EnvironmentContext,
        label: &str,
        module: &PythonModuleSourceValue,
    ) -> ValueResult {
        info!(
            &context.logger,
            "adding Python source module {}", module.inner.name;
        );
        self.exe
            .add_python_module_source(&module.inner, module.add_collection_context().clone())
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "PYOXIDIZER_BUILD",
                    message: e.to_string(),
                    label: label.to_string(),
                })
            })?;

        Ok(Value::new(NoneType::None))
    }

    pub fn add_python_package_resource(
        &mut self,
        context: &EnvironmentContext,
        label: &str,
        resource: &PythonPackageResourceValue,
    ) -> ValueResult {
        info!(
            &context.logger,
            "adding Python package resource {}",
            resource.inner.symbolic_name()
        );
        self.exe
            .add_python_package_resource(&resource.inner, resource.add_collection_context().clone())
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "PYOXIDIZER_BUILD",
                    message: e.to_string(),
                    label: label.to_string(),
                })
            })?;

        Ok(Value::new(NoneType::None))
    }

    pub fn add_python_package_distribution_resource(
        &mut self,
        context: &EnvironmentContext,
        label: &str,
        resource: &PythonPackageDistributionResourceValue,
    ) -> ValueResult {
        info!(
            &context.logger,
            "adding package distribution resource {}:{}",
            resource.inner.package,
            resource.inner.name
        );
        self.exe
            .add_python_package_distribution_resource(
                &resource.inner,
                resource.add_collection_context().clone(),
            )
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "PYOXIDIZER_BUILD",
                    message: e.to_string(),
                    label: label.to_string(),
                })
            })?;

        Ok(Value::new(NoneType::None))
    }

    pub fn add_python_extension_module(
        &mut self,
        context: &EnvironmentContext,
        label: &str,
        module: &PythonExtensionModuleValue,
    ) -> ValueResult {
        info!(
            &context.logger,
            "adding extension module {}", module.inner.name
        );
        self.exe
            .add_python_extension_module(&module.inner, module.add_collection_context().clone())
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "PYOXIDIZER_BUILD",
                    message: e.to_string(),
                    label: label.to_string(),
                })
            })?;

        Ok(Value::new(NoneType::None))
    }

    /// PythonExecutable.add_python_resource(resource)
    pub fn starlark_add_python_resource(
        &mut self,
        type_values: &TypeValues,
        resource: &Value,
        label: &str,
    ) -> ValueResult {
        let raw_context = get_context(type_values)?;
        let context = raw_context
            .downcast_ref::<EnvironmentContext>()
            .ok_or(ValueError::IncorrectParameterType)?;

        match resource.get_type() {
            "PythonModuleSource" => {
                let module = resource.downcast_ref::<PythonModuleSourceValue>().unwrap();
                self.add_python_module_source(context.deref(), label, module.deref())
            }
            "PythonPackageResource" => {
                let r = resource
                    .downcast_ref::<PythonPackageResourceValue>()
                    .unwrap();
                self.add_python_package_resource(context.deref(), label, r.deref())
            }
            "PythonPackageDistributionResource" => {
                let r = resource
                    .downcast_ref::<PythonPackageDistributionResourceValue>()
                    .unwrap();
                self.add_python_package_distribution_resource(context.deref(), label, r.deref())
            }
            "PythonExtensionModule" => {
                let module = resource
                    .downcast_ref::<PythonExtensionModuleValue>()
                    .unwrap();
                self.add_python_extension_module(context.deref(), label, module.deref())
            }
            _ => Err(ValueError::from(RuntimeError {
                code: INCORRECT_PARAMETER_TYPE_ERROR_CODE,
                message: "resource argument must be a Python resource type".to_string(),
                label: ".add_python_resource()".to_string(),
            })),
        }
    }

    /// PythonExecutable.add_python_resources(resources)
    pub fn starlark_add_python_resources(
        &mut self,
        type_values: &TypeValues,
        resources: &Value,
    ) -> ValueResult {
        for resource in &resources.iter()? {
            self.starlark_add_python_resource(type_values, &resource, "add_python_resources()")?;
        }

        Ok(Value::new(NoneType::None))
    }

    /// PythonExecutable.to_embedded_resources()
    pub fn starlark_to_embedded_resources(&self) -> ValueResult {
        Ok(Value::new(PythonEmbeddedResources {
            exe: self.exe.clone_box(),
        }))
    }

    /// PythonExecutable.filter_resources_from_files(files=None, glob_files=None)
    pub fn starlark_filter_resources_from_files(
        &mut self,
        type_values: &TypeValues,
        files: &Value,
        glob_files: &Value,
    ) -> ValueResult {
        optional_list_arg("files", "string", &files)?;
        optional_list_arg("glob_files", "string", &glob_files)?;

        let files = match files.get_type() {
            "list" => files
                .iter()?
                .iter()
                .map(|x| PathBuf::from(x.to_string()))
                .collect(),
            "NoneType" => Vec::new(),
            _ => panic!("type should have been validated above"),
        };

        let glob_files = match glob_files.get_type() {
            "list" => glob_files.iter()?.iter().map(|x| x.to_string()).collect(),
            "NoneType" => Vec::new(),
            _ => panic!("type should have been validated above"),
        };

        let files_refs = files.iter().map(|x| x.as_ref()).collect::<Vec<&Path>>();
        let glob_files_refs = glob_files.iter().map(|x| x.as_ref()).collect::<Vec<&str>>();

        let raw_context = get_context(type_values)?;
        let context = raw_context
            .downcast_ref::<EnvironmentContext>()
            .ok_or(ValueError::IncorrectParameterType)?;

        self.exe
            .filter_resources_from_files(&context.logger, &files_refs, &glob_files_refs)
            .map_err(|e| {
                ValueError::from(RuntimeError {
                    code: "RUNTIME_ERROR",
                    message: e.to_string(),
                    label: "filter_from_files()".to_string(),
                })
            })?;

        Ok(Value::new(NoneType::None))
    }
}

starlark_module! { python_executable_env =>
    #[allow(non_snake_case, clippy::ptr_arg)]
    PythonExecutable.make_python_module_source(
        env env,
        call_stack cs,
        this,
        name,
        source,
        is_package=false
    ) {
        match this.clone().downcast_ref::<PythonExecutable>() {
            Some(exe) => exe.starlark_make_python_module_source(&env, cs, &name, &source, &is_package),
            None => Err(ValueError::IncorrectParameterType),
        }
    }

    #[allow(non_snake_case, clippy::ptr_arg)]
    PythonExecutable.pip_download(
        env env,
        call_stack cs,
        this,
        args
    ) {
        match this.clone().downcast_ref::<PythonExecutable>() {
            Some(exe) => exe.starlark_pip_download(&env, cs, &args),
            None => Err(ValueError::IncorrectParameterType),
        }
    }

    #[allow(non_snake_case, clippy::ptr_arg)]
    PythonExecutable.pip_install(
        env env,
        call_stack cs,
        this,
        args,
        extra_envs=NoneType::None
    ) {
        match this.clone().downcast_ref::<PythonExecutable>() {
            Some(exe) => exe.starlark_pip_install(&env, cs, &args, &extra_envs),
            None => Err(ValueError::IncorrectParameterType),
        }
    }

    #[allow(non_snake_case, clippy::ptr_arg)]
    PythonExecutable.read_package_root(
        env env,
        call_stack cs,
        this,
        path,
        packages
    ) {
        match this.clone().downcast_ref::<PythonExecutable>() {
            Some(exe) => exe.starlark_read_package_root(&env, cs, &path, &packages),
            None => Err(ValueError::IncorrectParameterType),
        }
    }

    #[allow(non_snake_case, clippy::ptr_arg)]
    PythonExecutabvle.read_virtualenv(
        env env,
        call_stack cs,
        this,
        path
    ) {
        match this.clone().downcast_ref::<PythonExecutable>() {
            Some(exe) => exe.starlark_read_virtualenv(&env, cs, &path),
            None => Err(ValueError::IncorrectParameterType),
        }
    }

    #[allow(non_snake_case, clippy::ptr_arg)]
    PythonExecutable.setup_py_install(
        env env,
        call_stack cs,
        this,
        package_path,
        extra_envs=NoneType::None,
        extra_global_arguments=NoneType::None
    ) {
        match this.clone().downcast_ref::<PythonExecutable>() {
            Some(exe) => exe.starlark_setup_py_install(&env, cs, &package_path, &extra_envs, &extra_global_arguments),
            None => Err(ValueError::IncorrectParameterType),
        }
    }

    #[allow(non_snake_case, clippy::ptr_arg)]
    PythonExecutable.add_python_resource(
        env env,
        this,
        resource
    ) {
        match this.clone().downcast_mut::<PythonExecutable>()? {
            Some(mut exe) => exe.starlark_add_python_resource(
                &env,
                &resource,
                "add_python_resource",
            ),
            None => Err(ValueError::IncorrectParameterType),
        }
    }

    #[allow(non_snake_case, clippy::ptr_arg)]
    PythonExecutable.add_python_resources(
        env env,
        this,
        resources
    ) {
        match this.clone().downcast_mut::<PythonExecutable>()? {
            Some(mut exe) => exe.starlark_add_python_resources(
                &env,
                &resources,
            ),
            None => Err(ValueError::IncorrectParameterType),
        }
    }

    #[allow(clippy::ptr_arg)]
    PythonExecutable.filter_resources_from_files(
        env env,
        this,
        files=NoneType::None,
        glob_files=NoneType::None)
    {
        match this.clone().downcast_mut::<PythonExecutable>()? {
            Some(mut exe) => exe.starlark_filter_resources_from_files(&env, &files, &glob_files),
            None => Err(ValueError::IncorrectParameterType),
        }
    }

    #[allow(clippy::ptr_arg)]
    PythonExecutable.to_embedded_resources(this) {
        match this.clone().downcast_ref::<PythonExecutable>() {
            Some(exe) => exe.starlark_to_embedded_resources(),
            None => Err(ValueError::IncorrectParameterType),
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::super::testutil::*, super::*, crate::python_distributions::PYTHON_DISTRIBUTIONS};

    #[test]
    fn test_default_values() -> Result<()> {
        let mut env = StarlarkEnvironment::new_with_exe()?;
        let exe = env.eval("exe")?;

        assert_eq!(exe.get_type(), "PythonExecutable");

        let exe = exe.downcast_ref::<PythonExecutable>().unwrap();
        assert!(exe
            .exe
            .iter_resources()
            .any(|(_, r)| r.in_memory_source.is_some()));
        assert!(exe
            .exe
            .iter_resources()
            .all(|(_, r)| r.in_memory_resources.is_none()));

        Ok(())
    }

    #[test]
    fn test_no_sources() -> Result<()> {
        let mut env = StarlarkEnvironment::new()?;

        env.eval("dist = default_python_distribution()")?;
        env.eval("policy = dist.make_python_packaging_policy()")?;
        env.eval("policy.include_distribution_sources = False")?;

        let exe = env.eval("dist.to_python_executable('testapp', packaging_policy=policy)")?;

        assert_eq!(exe.get_type(), "PythonExecutable");

        let exe = exe.downcast_ref::<PythonExecutable>().unwrap();
        assert!(exe
            .exe
            .iter_resources()
            .all(|(_, r)| r.in_memory_source.is_none()));

        Ok(())
    }

    #[test]
    fn test_make_python_module_source() -> Result<()> {
        let mut env = StarlarkEnvironment::new_with_exe()?;
        let m = env.eval("exe.make_python_module_source('foo', 'import bar')")?;

        assert_eq!(m.get_type(), PythonModuleSourceValue::TYPE);
        assert_eq!(m.get_attr("name").unwrap().to_str(), "foo");
        assert_eq!(m.get_attr("source").unwrap().to_str(), "import bar");
        assert_eq!(m.get_attr("is_package").unwrap().to_bool(), false);

        Ok(())
    }

    #[test]
    fn test_make_python_module_source_callback() -> Result<()> {
        let mut env = StarlarkEnvironment::new()?;
        env.eval("dist = default_python_distribution()")?;
        env.eval("policy = dist.make_python_packaging_policy()")?;
        env.eval(
            "def my_func(policy, resource):\n    resource.add_source = True\n    resource.add_bytecode_optimization_level_two = True\n",
        )?;
        env.eval("policy.register_resource_callback(my_func)")?;
        env.eval("exe = dist.to_python_executable('testapp', packaging_policy = policy)")?;

        let m = env.eval("exe.make_python_module_source('foo', 'import bar')")?;

        assert_eq!(m.get_type(), PythonModuleSourceValue::TYPE);
        assert_eq!(m.get_attr("name").unwrap().to_str(), "foo");
        assert_eq!(m.get_attr("source").unwrap().to_str(), "import bar");
        assert_eq!(m.get_attr("is_package").unwrap().to_bool(), false);
        assert_eq!(m.get_attr("add_source").unwrap().to_bool(), true);
        assert_eq!(
            m.get_attr("add_bytecode_optimization_level_two")
                .unwrap()
                .to_bool(),
            true
        );

        Ok(())
    }

    #[test]
    fn test_pip_download_pyflakes() -> Result<()> {
        for target_triple in PYTHON_DISTRIBUTIONS.all_target_triples() {
            let mut env = StarlarkEnvironment::new()?;
            env.set_target_triple(target_triple)?;

            env.eval("dist = default_python_distribution()")?;
            env.eval("exe = dist.to_python_executable('testapp')")?;

            let resources = env.eval("exe.pip_download(['pyflakes==2.2.0'])")?;

            assert_eq!(resources.get_type(), "list");

            let raw_it = resources.iter().unwrap();
            let mut it = raw_it.iter();

            let v = it.next().unwrap();
            assert_eq!(v.get_type(), PythonModuleSourceValue::TYPE);
            let x = v.downcast_ref::<PythonModuleSourceValue>().unwrap();
            assert!(x.inner.package().starts_with("pyflakes"));
        }

        Ok(())
    }

    #[test]
    fn test_pip_install_simple() -> Result<()> {
        let mut env = StarlarkEnvironment::new()?;

        env.eval("dist = default_python_distribution()")?;
        env.eval("policy = dist.make_python_packaging_policy()")?;
        env.eval("policy.include_distribution_sources = False")?;
        env.eval("exe = dist.to_python_executable('testapp', packaging_policy = policy)")?;

        let resources = env.eval("exe.pip_install(['pyflakes==2.1.1'])")?;
        assert_eq!(resources.get_type(), "list");

        let raw_it = resources.iter().unwrap();
        let mut it = raw_it.iter();

        let v = it.next().unwrap();
        assert_eq!(v.get_type(), PythonModuleSourceValue::TYPE);
        let x = v.downcast_ref::<PythonModuleSourceValue>().unwrap();
        assert_eq!(x.inner.name, "pyflakes");
        assert!(x.inner.is_package);

        Ok(())
    }

    #[test]
    fn test_read_package_root_simple() -> Result<()> {
        let temp_dir = tempdir::TempDir::new("pyoxidizer-test")?;

        let root = temp_dir.path();
        std::fs::create_dir(root.join("bar"))?;
        let bar_init = root.join("bar").join("__init__.py");
        std::fs::write(&bar_init, "# bar")?;

        let foo_path = root.join("foo.py");
        std::fs::write(&foo_path, "# foo")?;

        let baz_path = root.join("baz.py");
        std::fs::write(&baz_path, "# baz")?;

        std::fs::create_dir(root.join("extra"))?;
        let extra_path = root.join("extra").join("__init__.py");
        std::fs::write(&extra_path, "# extra")?;

        let mut env = StarlarkEnvironment::new()?;
        env.eval("dist = default_python_distribution()")?;
        env.eval("policy = dist.make_python_packaging_policy()")?;
        env.eval("policy.include_distribution_sources = False")?;
        env.eval("exe = dist.to_python_executable('testapp', packaging_policy = policy)")?;

        let resources = env.eval(&format!(
            "exe.read_package_root(\"{}\", packages=['foo', 'bar'])",
            root.display()
        ))?;

        assert_eq!(resources.get_type(), "list");
        assert_eq!(resources.length().unwrap(), 2);

        let raw_it = resources.iter().unwrap();
        let mut it = raw_it.iter();

        let v = it.next().unwrap();
        assert_eq!(v.get_type(), PythonModuleSourceValue::TYPE);
        let x = v.downcast_ref::<PythonModuleSourceValue>().unwrap();
        assert_eq!(x.inner.name, "bar");
        assert!(x.inner.is_package);
        assert_eq!(x.inner.source.resolve().unwrap(), b"# bar");

        let v = it.next().unwrap();
        assert_eq!(v.get_type(), PythonModuleSourceValue::TYPE);
        let x = v.downcast_ref::<PythonModuleSourceValue>().unwrap();
        assert_eq!(x.inner.name, "foo");
        assert!(!x.inner.is_package);
        assert_eq!(x.inner.source.resolve().unwrap(), b"# foo");

        Ok(())
    }
}
