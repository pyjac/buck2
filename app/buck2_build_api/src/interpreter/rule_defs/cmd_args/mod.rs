/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::fmt::Debug;

use buck2_interpreter::types::label_relative_path::LabelRelativePath;
use buck2_interpreter::types::target_label::StarlarkTargetLabel;
use gazebo::prelude::*;
use starlark::environment::GlobalsBuilder;
use starlark::values::FrozenRef;
use starlark::values::FrozenValue;
use starlark::values::StringValue;
use starlark::values::Value;
use starlark::values::ValueLike;
use thiserror::Error;

use crate::attrs::resolve::attr_type::arg::value::ResolvedStringWithMacros;
use crate::interpreter::rule_defs::artifact::FrozenStarlarkOutputArtifact;
use crate::interpreter::rule_defs::artifact::StarlarkArtifact;
use crate::interpreter::rule_defs::cmd_args::options::QuoteStyle;
use crate::interpreter::rule_defs::provider::builtin::run_info::FrozenRunInfo;
use crate::interpreter::rule_defs::provider::builtin::run_info::RunInfo;
use crate::interpreter::rule_defs::transitive_set::FrozenTransitiveSetArgsProjection;

mod builder;
mod options;
#[cfg(test)]
mod test;
mod traits;
mod typ;

pub use builder::*;
pub use traits::*;
pub use typ::*;

#[derive(Debug, Error)]
enum CommandLineArgError {
    #[error(
        "expected command line item to be a string, artifact, or label, or list thereof, not `{repr}`"
    )]
    InvalidItemType { repr: String },
}

pub trait ValueAsCommandLineLike<'v> {
    fn as_command_line(&self) -> Option<&'v dyn CommandLineArgLike>;
    fn as_command_line_err(&self) -> anyhow::Result<&'v dyn CommandLineArgLike>;
}

pub(crate) trait ValueAsFrozenCommandLineLike {
    fn as_frozen_command_line(&self) -> Option<FrozenRef<'static, dyn FrozenCommandLineArgLike>>;
}

impl<'v> ValueAsCommandLineLike<'v> for Value<'v> {
    fn as_command_line(&self) -> Option<&'v dyn CommandLineArgLike> {
        if let Some(x) = self.to_value().unpack_starlark_str() {
            return Some(x as &dyn CommandLineArgLike);
        }

        macro_rules! check {
            ($t:ty) => {
                if let Some(v) = self.to_value().downcast_ref::<$t>() {
                    return Some(v as &dyn CommandLineArgLike);
                }
            };
        }

        // Typically downcasting is provided by implementing `StarlarkValue::provide`.
        // These are exceptions:
        // * either providers, where `StarlarkValue` is generated,
        //   and plugging in `provide` is tricky
        // * or live outside of `buck2_build_api` crate,
        //   so `impl StarlarkValue` cannot provide `CommandLineArgLike`
        check!(RunInfo);
        check!(FrozenRunInfo);
        check!(LabelRelativePath);
        check!(StarlarkTargetLabel);

        self.request_value()
    }

    fn as_command_line_err(&self) -> anyhow::Result<&'v dyn CommandLineArgLike> {
        self.as_command_line().ok_or_else(|| {
            CommandLineArgError::InvalidItemType {
                repr: self.to_value().to_repr(),
            }
            .into()
        })
    }
}

impl ValueAsFrozenCommandLineLike for FrozenValue {
    fn as_frozen_command_line(&self) -> Option<FrozenRef<'static, dyn FrozenCommandLineArgLike>> {
        if let Some(x) = self.downcast_frozen_starlark_str() {
            return Some(x.map(|s| s as &dyn FrozenCommandLineArgLike));
        }

        macro_rules! check {
            ($t:ty) => {
                if let Some(x) = self.downcast_frozen_ref::<$t>() {
                    return Some(x.map(|v| v as &dyn FrozenCommandLineArgLike));
                }
            };
        }

        check!(FrozenStarlarkCommandLine);
        check!(StarlarkArtifact);
        check!(FrozenStarlarkOutputArtifact);
        check!(ResolvedStringWithMacros);
        check!(FrozenRunInfo);
        check!(LabelRelativePath);
        check!(FrozenTransitiveSetArgsProjection);
        None
    }
}

#[starlark_module]
pub fn register_cmd_args(builder: &mut GlobalsBuilder) {
    #[starlark(type = "cmd_args")]
    /// The `cmd_args` type is created by this function and is consumed by `ctx.actions.run`.
    /// The type is a mutable collection of strings and artifact values.
    /// In general, command lines, artifacts, strings, `RunInfo` and lists thereof can be added to or used to construct a `cmd_args` value.
    ///
    /// The arguments are:
    ///
    /// * `*args` - a list of things to add to the command line, each of which must be coercible to a command line. Further items can be added with `cmd.add`.
    /// * `format` - a string that provides a format to apply to the argument. for example, `cmd_args(x, format="--args={}")` would prepend `--args=` before `x`, or if `x` was a list, before each element in `x`.
    /// * `delimiter` - added between arguments to join them together. For example, `cmd_args(["--args=",x], delimiter="")` would produce a single argument to the underlying tool.
    /// * `prepend` - added as a separate argument before each argument.
    /// * `quote` - indicates whether quoting is to be applied to each argument. The only current valid value is `"shell"`.
    fn cmd_args<'v>(
        #[starlark(args)] args: Vec<Value<'v>>,
        delimiter: Option<StringValue<'v>>,
        format: Option<StringValue<'v>>,
        prepend: Option<StringValue<'v>>,
        quote: Option<&str>,
    ) -> anyhow::Result<StarlarkCommandLine<'v>> {
        StarlarkCommandLine::try_from_values_with_options(
            &args,
            delimiter,
            format,
            prepend,
            quote.try_map(QuoteStyle::parse)?,
        )
    }
}

#[cfg(test)]
pub mod tester {
    use buck2_common::executor_config::PathSeparatorKind;
    use buck2_core::buck_path::resolver::BuckPathResolver;
    use buck2_core::fs::artifact_path_resolver::ArtifactFs;
    use buck2_core::fs::buck_out_path::BuckOutPathResolver;
    use buck2_core::fs::paths::abs_norm_path::AbsNormPathBuf;
    use buck2_core::fs::project::ProjectRoot;
    use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
    use buck2_execute::artifact::fs::ExecutorFs;
    use buck2_interpreter_for_build::interpreter::testing::cells;
    use starlark::environment::GlobalsBuilder;
    use starlark::values::Value;

    use crate::interpreter::rule_defs::cmd_args::builder::DefaultCommandLineContext;
    use crate::interpreter::rule_defs::cmd_args::ValueAsCommandLineLike;

    fn artifact_fs() -> ArtifactFs {
        let cell_info = cells(None).unwrap();
        ArtifactFs::new(
            BuckPathResolver::new(cell_info.1),
            BuckOutPathResolver::new(ProjectRelativePathBuf::unchecked_new(
                "buck-out/v2".to_owned(),
            )),
            ProjectRoot::new(AbsNormPathBuf::try_from(std::env::current_dir().unwrap()).unwrap())
                .unwrap(),
        )
    }

    fn get_command_line(value: Value) -> anyhow::Result<Vec<String>> {
        let fs = artifact_fs();
        let executor_fs = ExecutorFs::new(&fs, PathSeparatorKind::Unix);
        let mut cli = Vec::<String>::new();
        let mut ctx = DefaultCommandLineContext::new(&executor_fs);

        match value.as_command_line() {
            Some(v) => v.add_to_command_line(&mut cli, &mut ctx),
            None => value
                .as_command_line_err()?
                .add_to_command_line(&mut cli, &mut ctx),
        }?;
        Ok(cli)
    }

    #[starlark_module]
    pub fn command_line_stringifier(builder: &mut GlobalsBuilder) {
        fn get_args<'v>(value: Value<'v>) -> anyhow::Result<Vec<String>> {
            get_command_line(value)
        }

        fn stringify_cli_arg<'v>(value: Value<'v>) -> anyhow::Result<String> {
            let fs = artifact_fs();
            let executor_fs = ExecutorFs::new(&fs, PathSeparatorKind::Unix);
            let mut cli = Vec::<String>::new();
            let mut ctx = DefaultCommandLineContext::new(&executor_fs);
            value
                .as_command_line_err()?
                .add_to_command_line(&mut cli, &mut ctx)?;
            assert_eq!(1, cli.len());
            Ok(cli.get(0).unwrap().clone())
        }
    }
}
