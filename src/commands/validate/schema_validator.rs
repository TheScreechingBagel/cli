use std::{
    collections::HashSet,
    path::Path,
    sync::{Arc, LazyLock},
};

use blue_build_process_management::ASYNC_RUNTIME;
use bon::bon;
use cached::proc_macro::cached;
use colored::Colorize;
use indexmap::IndexMap;
use jsonschema::{BasicOutput, Retrieve, Uri, ValidationError, Validator};
use log::trace;
use miette::{miette, Context, IntoDiagnostic, LabeledSpan, NamedSource, Report, Result};
use regex::Regex;
use serde_json::Value;

use super::{location::Location, yaml_span::YamlSpan};

pub const RECIPE_V1_SCHEMA_URL: &str = "https://schema.blue-build.org/recipe-v1.json";
pub const STAGE_V1_SCHEMA_URL: &str = "https://schema.blue-build.org/stage-v1.json";
pub const MODULE_V1_SCHEMA_URL: &str = "https://schema.blue-build.org/module-v1.json";
pub const MODULE_STAGE_LIST_V1_SCHEMA_URL: &str =
    "https://schema.blue-build.org/module-stage-list-v1.json";

#[derive(Debug, Clone)]
pub struct SchemaValidator {
    #[expect(dead_code)]
    schema: Arc<Value>,
    validator: Arc<Validator>,
    url: &'static str,
    all_errors: bool,
}

#[bon]
impl SchemaValidator {
    #[builder]
    pub async fn new(
        /// The URL of the schema to validate against
        url: &'static str,
        /// Produce all errors found
        #[builder(default)]
        all_errors: bool,
    ) -> Result<Self, Report> {
        tokio::spawn(async move {
            let schema: Arc<Value> = Arc::new({
                #[cfg(not(test))]
                {
                    reqwest::get(url)
                        .await
                        .into_diagnostic()
                        .with_context(|| format!("Failed to get schema at {url}"))?
                        .json()
                        .await
                        .into_diagnostic()
                        .with_context(|| format!("Failed to get json for schema {url}"))?
                }
                #[cfg(test)]
                {
                    serde_json::from_slice(
                        std::fs::read_to_string(url)
                            .into_diagnostic()
                            .context("Failed retrieving initial schema")?
                            .as_bytes(),
                    )
                    .into_diagnostic()
                    .context("Failed deserializing initial schema")?
                }
            });
            let validator = Arc::new(
                tokio::task::spawn_blocking({
                    let schema = schema.clone();
                    move || {
                        jsonschema::options()
                            .with_retriever(ModuleSchemaRetriever)
                            .build(&schema)
                            .into_diagnostic()
                            .with_context(|| format!("Failed to build validator for schema {url}"))
                    }
                })
                .await
                .expect("Should join blocking thread")?,
            );

            Ok(Self {
                schema,
                validator,
                url,
                all_errors,
            })
        })
        .await
        .expect("Should join task")
    }

    pub fn process_validation<P>(&self, path: P, file: Arc<String>) -> Result<Option<Report>>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        let spans = self.get_spans(&file, path)?;

        Ok(self.spans_to_report(spans, file, path))
    }

    fn get_spans(&self, file: &Arc<String>, path: &Path) -> Result<Vec<LabeledSpan>> {
        let recipe_path_display = path.display().to_string().bold().italic();
        let spanner = YamlSpan::builder().file(file.clone()).build()?;
        let instance: Value = serde_yaml::from_str(file)
            .into_diagnostic()
            .with_context(|| format!("Failed to deserialize recipe {recipe_path_display}"))?;
        trace!("{recipe_path_display}:\n{file}");

        Ok(if self.all_errors {
            process_basic_output(self.validator.apply(&instance).basic(), &spanner)
        } else {
            process_err(self.validator.iter_errors(&instance), &spanner)
        })
    }

    fn spans_to_report(
        &self,
        spans: Vec<LabeledSpan>,
        file: Arc<String>,
        path: &Path,
    ) -> Option<Report> {
        if spans.is_empty() {
            None
        } else {
            Some(
                miette!(
                    labels = spans,
                    help = format!(
                        "Try adding these lines to the top of your file:\n{}\n{}",
                        "---".bright_green(),
                        format!("# yaml-language-server: $schema={}", self.url).bright_green(),
                    ),
                    "{} error{} encountered",
                    spans.len().to_string().red(),
                    if spans.len() == 1 { "" } else { "s" }
                )
                .with_source_code(
                    NamedSource::new(path.display().to_string(), file).with_language("yaml"),
                ),
            )
        }
    }
}

fn process_basic_output(out: BasicOutput<'_>, spanner: &YamlSpan) -> Vec<LabeledSpan> {
    match out {
        BasicOutput::Valid(_) => Vec::new(),
        BasicOutput::Invalid(errors) => {
            let errors = {
                let mut e = errors
                    .into_iter()
                    .inspect(|err| trace!("{err:?}"))
                    .collect::<Vec<_>>();
                e.sort_by(|e1, e2| {
                    e1.instance_location()
                        .as_str()
                        .cmp(e2.instance_location().as_str())
                });
                e
            };

            let mut collection: IndexMap<Location, HashSet<String>> = IndexMap::new();

            for err in errors {
                let err_msg = remove_json(err.error_description()).bold().red();
                collection
                    .entry(Location::from(err.instance_location()))
                    .and_modify(|errs| {
                        errs.insert(format!("- {err_msg}"));
                    })
                    .or_insert_with(|| {
                        let mut set = HashSet::new();
                        set.insert(format!("- {err_msg}"));
                        set
                    });
            }

            collection
                .into_iter()
                .map(|(key, value)| {
                    LabeledSpan::new_with_span(
                        Some(value.into_iter().collect::<Vec<_>>().join("\n")),
                        spanner.get_span(&key).unwrap(),
                    )
                })
                .collect()
        }
    }
}

fn process_err<'a, I>(errors: I, spanner: &YamlSpan) -> Vec<LabeledSpan>
where
    I: Iterator<Item = ValidationError<'a>>,
{
    errors
        .map(|err| {
            LabeledSpan::new_primary_with_span(
                Some(remove_json(&err).bold().red().to_string()),
                spanner
                    .get_span(&Location::from(err.instance_path))
                    .unwrap(),
            )
        })
        .collect()
}

fn remove_json<S>(string: &S) -> String
where
    S: ToString,
{
    static REGEX_OBJECT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\{.*\}\s(.*)$").unwrap());
    static REGEX_ARRAY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[.*\]\s(.*)$").unwrap());

    let string = string.to_string();

    if REGEX_OBJECT.is_match(&string) {
        REGEX_OBJECT.replace_all(string.trim(), "$1").into_owned()
    } else if REGEX_ARRAY.is_match(&string) {
        REGEX_ARRAY.replace_all(string.trim(), "$1").into_owned()
    } else {
        string
    }
}

struct ModuleSchemaRetriever;

impl Retrieve for ModuleSchemaRetriever {
    fn retrieve(
        &self,
        uri: &Uri<&str>,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        Ok(ASYNC_RUNTIME.block_on(cache_retrieve(uri))?)
    }
}

#[cached(result = true, key = "String", convert = r#"{ format!("{uri}") }"#)]
async fn cache_retrieve(uri: &Uri<&str>) -> miette::Result<Value> {
    let scheme = uri.scheme();
    let path = uri.path();

    #[cfg(not(test))]
    {
        const BASE_SCHEMA_URL: &str = "https://schema.blue-build.org";

        let uri = match scheme.as_str() {
            "json-schema" => {
                format!("{BASE_SCHEMA_URL}{path}")
            }
            "https" => uri.to_string(),
            scheme => miette::bail!("Unknown scheme {scheme}"),
        };

        log::debug!("Retrieving schema from {}", uri.bold().italic());
        tokio::spawn(async move {
            reqwest::get(&uri)
                .await
                .into_diagnostic()
                .with_context(|| format!("Failed to retrieve schema from {uri}"))?
                .json()
                .await
                .into_diagnostic()
                .with_context(|| format!("Failed to parse json from {uri}"))
                .inspect(|value| trace!("{}:\n{value}", uri.bold().italic()))
        })
        .await
        .expect("Should join task")
    }

    #[cfg(test)]
    {
        let uri = match scheme.as_str() {
            "json-schema" | "https" => {
                format!("test-files/schema/{path}")
            }
            _ => unreachable!(),
        };

        serde_json::from_slice(
            std::fs::read_to_string(uri)
                .into_diagnostic()
                .context("Failed retrieving sub-schema")?
                .as_bytes(),
        )
        .into_diagnostic()
        .context("Failed deserializing sub-schema")
    }
}

#[cfg(test)]
mod test {
    use blue_build_process_management::ASYNC_RUNTIME;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::recipe(
        "test-files/recipes/recipe-pass.yml",
        "test-files/schema/recipe-v1.json"
    )]
    #[case::stage("test-files/recipes/stage-pass.yml", "test-files/schema/stage-v1.json")]
    #[case::stage_list(
        "test-files/recipes/stage-list-pass.yml",
        "test-files/schema/stage-list-v1.json"
    )]
    #[case::module_list(
        "test-files/recipes/module-list-pass.yml",
        "test-files/schema/module-list-v1.json"
    )]
    #[case::akmods(
        "test-files/recipes/modules/akmods-pass.yml",
        "test-files/schema/modules/akmods.json"
    )]
    #[case::bling(
        "test-files/recipes/modules/bling-pass.yml",
        "test-files/schema/modules/bling.json"
    )]
    #[case::brew(
        "test-files/recipes/modules/brew-pass.yml",
        "test-files/schema/modules/brew.json"
    )]
    #[case::chezmoi(
        "test-files/recipes/modules/chezmoi-pass.yml",
        "test-files/schema/modules/chezmoi.json"
    )]
    #[case::containerfile(
        "test-files/recipes/modules/containerfile-pass.yml",
        "test-files/schema/modules/containerfile.json"
    )]
    #[case::copy(
        "test-files/recipes/modules/copy-pass.yml",
        "test-files/schema/modules/copy.json"
    )]
    #[case::default_flatpaks(
        "test-files/recipes/modules/default-flatpaks-pass.yml",
        "test-files/schema/modules/default-flatpaks.json"
    )]
    #[case::files(
        "test-files/recipes/modules/files-pass.yml",
        "test-files/schema/modules/files.json"
    )]
    #[case::fonts(
        "test-files/recipes/modules/fonts-pass.yml",
        "test-files/schema/modules/fonts.json"
    )]
    #[case::gnome_extensions(
        "test-files/recipes/modules/gnome-extensions-pass.yml",
        "test-files/schema/modules/gnome-extensions.json"
    )]
    #[case::gschema_overrides(
        "test-files/recipes/modules/gschema-overrides-pass.yml",
        "test-files/schema/modules/gschema-overrides.json"
    )]
    #[case::justfiles(
        "test-files/recipes/modules/justfiles-pass.yml",
        "test-files/schema/modules/justfiles.json"
    )]
    #[case::rpm_ostree(
        "test-files/recipes/modules/rpm-ostree-pass.yml",
        "test-files/schema/modules/rpm-ostree.json"
    )]
    #[case::script(
        "test-files/recipes/modules/script-pass.yml",
        "test-files/schema/modules/script.json"
    )]
    #[case::signing(
        "test-files/recipes/modules/signing-pass.yml",
        "test-files/schema/modules/signing.json"
    )]
    #[case::systemd(
        "test-files/recipes/modules/systemd-pass.yml",
        "test-files/schema/modules/systemd.json"
    )]
    #[case::yafti(
        "test-files/recipes/modules/yafti-pass.yml",
        "test-files/schema/modules/yafti.json"
    )]
    fn pass_validation(#[case] file: &str, #[case] schema: &'static str) {
        let validator = ASYNC_RUNTIME
            .block_on(SchemaValidator::builder().url(schema).build())
            .unwrap();

        let file_contents = Arc::new(std::fs::read_to_string(file).unwrap());

        let result = validator.process_validation(file, file_contents).unwrap();
        dbg!(&result);

        assert!(result.is_none());
    }

    #[rstest]
    #[case::recipe(
        "test-files/recipes/recipe-fail.yml",
        "test-files/schema/recipe-v1.json"
    )]
    #[case::stage("test-files/recipes/stage-fail.yml", "test-files/schema/stage-v1.json")]
    #[case::stage_list(
        "test-files/recipes/stage-list-fail.yml",
        "test-files/schema/stage-list-v1.json"
    )]
    #[case::module_list(
        "test-files/recipes/module-list-fail.yml",
        "test-files/schema/module-list-v1.json"
    )]
    #[case::akmods(
        "test-files/recipes/modules/akmods-fail.yml",
        "test-files/schema/modules/akmods.json"
    )]
    #[case::bling(
        "test-files/recipes/modules/bling-fail.yml",
        "test-files/schema/modules/bling.json"
    )]
    #[case::brew(
        "test-files/recipes/modules/brew-fail.yml",
        "test-files/schema/modules/brew.json"
    )]
    #[case::chezmoi(
        "test-files/recipes/modules/chezmoi-fail.yml",
        "test-files/schema/modules/chezmoi.json"
    )]
    #[case::containerfile(
        "test-files/recipes/modules/containerfile-fail.yml",
        "test-files/schema/modules/containerfile.json"
    )]
    #[case::copy(
        "test-files/recipes/modules/copy-fail.yml",
        "test-files/schema/modules/copy.json"
    )]
    #[case::default_flatpaks(
        "test-files/recipes/modules/default-flatpaks-fail.yml",
        "test-files/schema/modules/default-flatpaks.json"
    )]
    #[case::files(
        "test-files/recipes/modules/files-fail.yml",
        "test-files/schema/modules/files.json"
    )]
    #[case::fonts(
        "test-files/recipes/modules/fonts-fail.yml",
        "test-files/schema/modules/fonts.json"
    )]
    #[case::gnome_extensions(
        "test-files/recipes/modules/gnome-extensions-fail.yml",
        "test-files/schema/modules/gnome-extensions.json"
    )]
    #[case::gschema_overrides(
        "test-files/recipes/modules/gschema-overrides-fail.yml",
        "test-files/schema/modules/gschema-overrides.json"
    )]
    #[case::justfiles(
        "test-files/recipes/modules/justfiles-fail.yml",
        "test-files/schema/modules/justfiles.json"
    )]
    #[case::rpm_ostree(
        "test-files/recipes/modules/rpm-ostree-fail.yml",
        "test-files/schema/modules/rpm-ostree.json"
    )]
    #[case::script(
        "test-files/recipes/modules/script-fail.yml",
        "test-files/schema/modules/script.json"
    )]
    #[case::signing(
        "test-files/recipes/modules/signing-fail.yml",
        "test-files/schema/modules/signing.json"
    )]
    #[case::systemd(
        "test-files/recipes/modules/systemd-fail.yml",
        "test-files/schema/modules/systemd.json"
    )]
    #[case::yafti(
        "test-files/recipes/modules/yafti-fail.yml",
        "test-files/schema/modules/yafti.json"
    )]
    fn fail_validation(#[case] file: &str, #[case] schema: &'static str) {
        let validator = ASYNC_RUNTIME
            .block_on(SchemaValidator::builder().url(schema).build())
            .unwrap();

        let file_contents = Arc::new(std::fs::read_to_string(file).unwrap());

        let result = validator.process_validation(file, file_contents).unwrap();
        dbg!(&result);

        assert!(result.is_some());
    }
}
