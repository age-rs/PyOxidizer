.. _config_type_python_executable:

====================
``PythonExecutable``
====================

The ``PythonExecutable`` type represents an executable file containing
the Python interpreter, Python resources to make available to the interpreter,
and a default run-time configuration for that interpreter.

Instances are constructed from :ref:`config_type_python_distribution`
instances using
:ref:`config_python_distribution_to_python_executable`.

Methods
=======

.. _config_python_executable_make_python_module_source:

``PythonExecutable.make_python_module_source()``
------------------------------------------------

This method creates a :ref:`config_type_python_module_source` instance
suitable for use with the executable being built.

Arguments are as follows:

``name`` (string)
   The name of the Python module. This is the fully qualified module
   name. e.g. ``foo`` or ``foo.bar``.
``source`` (string)
   Python source code comprising the module.
``is_package`` (bool)
   Whether the Python module is also a package. (e.g. the equivalent of a
   ``__init__.py`` file or a module without a ``.`` in its name.

.. _config_python_executable_pip_download:

``PythonExecutable.pip_download()``
-----------------------------------

This method runs ``pip download <args>`` with settings appropriate to target
the executable being built.

This always uses ``--only-binary=:all:``, forcing pip to only download wheel
based packages.

This method accepts the following arguments:

``args``
   (``list`` of ``string``) Command line arguments to pass to ``pip download``.
   Arguments will be added after default arguments added internally.

Returns a ``list`` of objects representing Python resources collected
from wheels obtained via ``pip download``.

.. _config_python_executable_pip_install:

``PythonExecutable.pip_install()``
----------------------------------

This method runs ``pip install <args>`` with settings appropriate to target
the executable being built.

``args``
   List of strings defining raw process arguments to pass to ``pip install``.

``extra_envs``
   Optional dict of string key-value pairs constituting extra environment
   variables to set in the invoked ``pip`` process.

Returns a ``list`` of objects representing Python resources installed as
part of the operation. The types of these objects can be
:ref:`config_type_python_module_source`,
:ref:`config_type_python_package_resource`, etc.

The returned resources are typically added to a ``FileManifest`` or
``PythonExecutable`` to make them available to a packaged
application.

.. _config_python_executable_read_package_root:

``PythonExecutable.read_package_root()``
----------------------------------------

This method discovers resources from a directory on the filesystem.

The specified directory will be scanned for resource files. However,
only specific named *packages* will be found. e.g. if the directory
contains sub-directories ``foo/`` and ``bar``, you must explicitly
state that you want the ``foo`` and/or ``bar`` package to be included
so files from these directories will be read.

This rule is frequently used to pull in packages from local source
directories (e.g. directories containing a ``setup.py`` file). This
rule doesn't involve any packaging tools and is a purely driven by
filesystem walking. It is primitive, yet effective.

This rule has the following arguments:

``path`` (string)
   The filesystem path to the directory to scan.

``packages`` (list of string)
   List of package names to include.

   Filesystem walking will find files in a directory ``<path>/<value>/`` or in
   a file ``<path>/<value>.py``.

Returns a ``list`` of objects representing Python resources found in the
virtualenv. The types of these objects can be ``PythonModuleSource``,
``PythonPackageResource``, etc.

The returned resources are typically added to a ``FileManifest`` or
``PythonExecutable`` to make them available to a packaged application.

.. _config_python_executable_read_virtualenv:

``PythonExecutable.read_virtualenv()``
--------------------------------------

This method attempts to read Python resources from an already built
virtualenv.

.. important::

   PyOxidizer only supports finding modules and resources
   populated via *traditional* means (e.g. ``pip install`` or ``python setup.py
   install``). If ``.pth`` or similar mechanisms are used for installing modules,
   files may not be discovered properly.

It accepts the following arguments:

``path`` (string)
   The filesystem path to the root of the virtualenv.

   Python modules are typically in a ``lib/pythonX.Y/site-packages`` directory
   (on UNIX) or ``Lib/site-packages`` directory (on Windows) under this path.

Returns a ``list`` of objects representing Python resources found in the virtualenv.
The types of these objects can be ``PythonModuleSource``,
``PythonPackageResource``, etc.

The returned resources are typically added to a ``FileManifest`` or
``PythonExecutable`` to make them available to a packaged application.

.. _config_python_executable_setup_py_install:

``PythonExecutable.setup_py_install()``
---------------------------------------

This method runs ``python setup.py install`` against a package at the
specified path.

It accepts the following arguments:

``package_path``
   String filesystem path to directory containing a ``setup.py`` to invoke.

``extra_envs={}``
   Optional dict of string key-value pairs constituting extra environment
   variables to set in the invoked ``python`` process.

``extra_global_arguments=[]``
   Optional list of strings of extra command line arguments to pass to
   ``python setup.py``. These will be added before the ``install``
   argument.

Returns a ``list`` of objects representing Python resources installed
as part of the operation. The types of these objects can be
``PythonModuleSource``, ``PythonPackageResource``, etc.

The returned resources are typically added to a ``FileManifest`` or
``PythonExecutable`` to make them available to a packaged application.

.. _config_python_executable_add_python_resource:

``PythonExecutable.add_python_resource()``
------------------------------------------

This method registers a Python resource of various types with the instance.

It accepts a ``resource`` argument which can be a ``PythonModuleSource``,
``PythonPackageResource``, or ``PythonExtensionModule`` and registers that
resource with this instance.

The following arguments are accepted:

``resource``
   The resource to add to the embedded Python environment.

This method is a glorified proxy to the various ``add_python_*`` methods.
Unlike those methods, this one accepts all types that are known Python
resources.

.. _config_python_executable_add_python_resources:

``PythonExecutable.add_python_resources()``
-------------------------------------------

This method registers an iterable of Python resources of various types.
This method is identical to
:ref:`config_python_executable_add_python_resource` except the argument is
an iterable of resources. All other arguments are identical.

.. _config_python_executable_filter_from_files:

``PythonExecutable.filter_from_files()``
----------------------------------------

This method filters all embedded resources (source modules, bytecode modules,
and resource names) currently present on the instance through a set of
resource names resolved from files.

This method accepts the following arguments:

``files`` (array of string)
   List of filesystem paths to files containing resource names. The file
   must be valid UTF-8 and consist of a ``\n`` delimited list of resource
   names. Empty lines and lines beginning with ``#`` are ignored.

``glob_files`` (array of string)
   List of glob matching patterns of filter files to read. ``*`` denotes
   all files in a directory. ``**`` denotes recursive directories. This
   uses the Rust ``glob`` crate under the hood and the documentation for that
   crate contains more pattern matching info.

   The files read by this argument must be the same format as documented
   by the ``files`` argument.

All defined files are first read and the resource names encountered are
unioned into a set. This set is then used to filter entities currently
registered with the instance.

.. _config_python_executable_to_embedded_resources:

``PythonExecutable.to_embedded_resources()``
--------------------------------------------

Obtains a :ref:`config_type_python_embedded_resources` instance representing
resources to be made available to the Python interpreter.

See the :ref:`config_type_python_embedded_resources` type documentation for more.
