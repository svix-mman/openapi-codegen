use std::{
    collections::{BTreeMap, BTreeSet},
    io::BufWriter,
    path::{Path, PathBuf},
};

use aide::openapi::{self, ReferenceOr};
use anyhow::{bail, Context as _};
use fs_err::{self as fs, File};
use heck::ToSnakeCase as _;
use indexmap::IndexMap;
use minijinja::context;
use schemars::schema::{InstanceType, Schema};
use serde::Serialize;

use crate::{
    template,
    types::{FieldType, Types},
    util::get_schema_name,
};

/// The API we generate a client for.
///
/// Intermediate representation of `paths` from the spec.
#[derive(Debug)]
pub(crate) struct Api {
    resources: BTreeMap<String, Resource>,
}

impl Api {
    pub(crate) fn new(paths: openapi::Paths) -> anyhow::Result<Self> {
        let mut resources = BTreeMap::new();

        for (path, pi) in paths {
            let path_item = pi
                .into_item()
                .context("$ref paths are currently not supported")?;

            if !path_item.parameters.is_empty() {
                tracing::info!("parameters at the path item level are not currently supported");
                continue;
            }

            for (method, op) in path_item {
                if let Some((res_name, op)) = Operation::from_openapi(&path, method, op) {
                    let resource = resources
                        .entry(res_name.clone())
                        .or_insert_with(|| Resource::new(res_name));
                    resource.operations.push(op);
                }
            }
        }

        Ok(Self { resources })
    }

    fn referenced_components(&self) -> impl Iterator<Item = &str> {
        self.resources
            .values()
            .flat_map(|resource| &resource.operations)
            .filter_map(|operation| operation.request_body_schema_name.as_deref())
    }

    pub(crate) fn types(&self, schemas: &mut IndexMap<String, openapi::SchemaObject>) -> Types {
        let components: BTreeSet<_> = self.referenced_components().collect();
        Types(
            components
                .into_iter()
                .filter_map(|schema_name| {
                    let Some(s) = schemas.swap_remove(schema_name) else {
                        tracing::warn!(schema_name, "schema not found");
                        return None;
                    };
                    match s.json_schema {
                        Schema::Bool(_) => {
                            tracing::warn!("found $ref'erenced bool schema, wat?!");
                            None
                        }
                        Schema::Object(schema_object) => {
                            Some((schema_name.to_owned(), schema_object))
                        }
                    }
                })
                .collect(),
        )
    }

    pub(crate) fn write_rust_stuff(
        self,
        output_dir: impl AsRef<Path>,
        no_format: bool,
    ) -> anyhow::Result<()> {
        let output_dir = output_dir.as_ref();

        fs::write(
            output_dir.join(".rustfmt.toml"),
            include_str!("../.rustfmt.toml"),
        )?;

        let minijinja_env = template::env()?;
        let lib_resource_tpl = minijinja_env.get_template("svix_lib_resource.rs.jinja")?;
        let cli_resource_tpl = minijinja_env.get_template("svix_cli_resource.rs.jinja")?;
        let cli_types_tpl = minijinja_env.get_template("svix_cli_types.rs.jinja")?;

        let api_dir = output_dir.join("api");
        let cli_api_dir = output_dir.join("cli_api");
        let cli_types_dir = output_dir.join("cli_types");
        fs::create_dir(&api_dir)?;
        fs::create_dir(&cli_api_dir)?;
        fs::create_dir(&cli_types_dir)?;

        for (name, resource) in self.resources {
            let filename = format!("{}.rs", name.to_snake_case());
            let ctx = context! { resource => resource };
            let do_write = |path: PathBuf, tpl| write_rust(&path, tpl, &ctx, no_format);

            do_write(api_dir.join(&filename), &lib_resource_tpl)?;
            do_write(cli_api_dir.join(&filename), &cli_resource_tpl)?;
            do_write(cli_types_dir.join(&filename), &cli_types_tpl)?;
        }

        Ok(())
    }
}

fn write_rust(
    path: &Path,
    tpl: &minijinja::Template<'_, '_>,
    ctx: impl Serialize,
    no_format: bool,
) -> anyhow::Result<()> {
    let out_file = BufWriter::new(File::create(path)?);
    tpl.render_to_write(&ctx, out_file)?;

    if !no_format {
        _ = std::process::Command::new("rustfmt")
            .args(["+nightly", "--edition", "2021"])
            .arg(path)
            .status();
    }

    Ok(())
}

/// A named group of [`Operation`]s.
#[derive(Debug, serde::Serialize)]
struct Resource {
    name: String,
    operations: Vec<Operation>,
    // TODO: subresources?
}

impl Resource {
    fn new(name: String) -> Self {
        Self {
            name,
            operations: Vec::new(),
        }
    }
}

/// A named HTTP endpoint.
#[derive(Debug, serde::Serialize)]
struct Operation {
    /// The operation ID from the spec.
    id: String,
    /// The name to use for the operation in code.
    name: String,
    /// Description of the operation to use for documentation.
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    /// The HTTP method.
    ///
    /// Encoded as "get", "post" or such because that's what aide's PathItem iterator gives us.
    method: String,
    /// The operation's endpoint path.
    path: String,
    /// Path parameters.
    ///
    /// Only required string-typed parameters are currently supported.
    path_params: Vec<String>,
    /// Header parameters.
    ///
    /// Only string-typed parameters are currently supported.
    header_params: Vec<HeaderParam>,
    /// Query parameters.
    query_params: Vec<QueryParam>,
    /// Name of the request body type, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    request_body_schema_name: Option<String>,
    /// Name of the response body type, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    response_body_schema_name: Option<String>,
}

impl Operation {
    #[tracing::instrument(name = "operation_from_openapi", skip(op), fields(op_id))]
    fn from_openapi(path: &str, method: &str, op: openapi::Operation) -> Option<(String, Self)> {
        let Some(op_id) = op.operation_id else {
            // ignore operations without an operationId
            return None;
        };
        let op_id_parts: Vec<_> = op_id.split(".").collect();
        let Ok([version, res_name, op_name]): Result<[_; 3], _> = op_id_parts.try_into() else {
            tracing::debug!(op_id, "skipping operation whose ID does not have two dots");
            return None;
        };
        if version != "v1" {
            tracing::warn!(op_id, "found operation whose ID does not begin with v1");
            return None;
        }

        let mut path_params = Vec::new();
        let mut query_params = Vec::new();
        let mut header_params = Vec::new();

        for param in op.parameters {
            match param {
                ReferenceOr::Reference { .. } => {
                    tracing::warn!("$ref parameters are not currently supported");
                    return None;
                }
                ReferenceOr::Item(openapi::Parameter::Path {
                    parameter_data,
                    style: openapi::PathStyle::Simple,
                }) => {
                    assert!(parameter_data.required, "no optional path params");
                    if let Err(e) = enforce_string_parameter(&parameter_data) {
                        tracing::warn!("unsupported path parameter: {e}");
                        return None;
                    }

                    path_params.push(parameter_data.name);
                }
                ReferenceOr::Item(openapi::Parameter::Header {
                    parameter_data,
                    style: openapi::HeaderStyle::Simple,
                }) => {
                    if let Err(e) = enforce_string_parameter(&parameter_data) {
                        tracing::warn!("unsupported header parameter: {e}");
                        return None;
                    }

                    header_params.push(HeaderParam {
                        name: parameter_data.name,
                        required: parameter_data.required,
                    });
                }
                ReferenceOr::Item(openapi::Parameter::Query {
                    parameter_data,
                    allow_reserved: false,
                    style: openapi::QueryStyle::Form,
                    allow_empty_value: None,
                }) => {
                    let name = parameter_data.name;
                    let _guard = tracing::info_span!("field_type_from_openapi", name).entered();
                    let r#type = match FieldType::from_openapi(parameter_data.format) {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::warn!("unsupport query parameter type: {e}");
                            return None;
                        }
                    };

                    query_params.push(QueryParam {
                        name,
                        description: parameter_data.description,
                        required: parameter_data.required,
                        r#type,
                    });
                }
                ReferenceOr::Item(parameter) => {
                    tracing::warn!(
                        ?parameter,
                        "this kind of parameter is not currently supported"
                    );
                    return None;
                }
            }
        }

        let request_body_schema_name = op.request_body.and_then(|b| match b {
            ReferenceOr::Item(mut req_body) => {
                assert!(req_body.required);
                assert!(req_body.extensions.is_empty());
                assert_eq!(req_body.content.len(), 1);
                let json_body = req_body
                    .content
                    .swap_remove("application/json")
                    .expect("should have JSON body");
                assert!(json_body.extensions.is_empty());
                match json_body.schema.expect("no json body schema?!").json_schema {
                    Schema::Bool(_) => {
                        tracing::error!("unexpected bool schema");
                        None
                    }
                    Schema::Object(obj) => {
                        if !obj.is_ref() {
                            tracing::error!(?obj, "unexpected non-$ref json body schema");
                        }
                        get_schema_name(obj.reference)
                    }
                }
            }
            ReferenceOr::Reference { .. } => {
                tracing::error!("$ref request bodies are not currently supported");
                None
            }
        });

        let response_body_schema_name = op.responses.and_then(|r| {
            assert_eq!(r.default, None);
            assert!(r.extensions.is_empty());
            let mut success_responses = r.responses.into_iter().filter(|(st, _)| {
                match st {
                    openapi::StatusCode::Code(c) => match c {
                        0..100 => tracing::error!("invalid status code < 100"),
                        100..200 => tracing::error!("what is this? status code {c}..."),
                        200..300 => return true,
                        300..400 => tracing::error!("what is this? status code {c}..."),
                        400.. => {}
                    },
                    openapi::StatusCode::Range(_) => {
                        tracing::error!("unsupported status code range");
                    }
                }

                false
            });

            let (_, resp) = success_responses
                .next()
                .expect("every operation must have one success response");
            let schema_name = response_body_schema_name(resp);
            for (_, resp) in success_responses {
                assert_eq!(schema_name, response_body_schema_name(resp));
            }

            schema_name
        });

        let res_name = res_name.to_owned();
        let op_name = op_name.to_owned();

        let op = Operation {
            id: op_id,
            name: op_name,
            description: op.description,
            method: method.to_owned(),
            path: path.to_owned(),
            path_params,
            header_params,
            query_params,
            request_body_schema_name,
            response_body_schema_name,
        };
        Some((res_name, op))
    }
}

fn enforce_string_parameter(parameter_data: &openapi::ParameterData) -> anyhow::Result<()> {
    let openapi::ParameterSchemaOrContent::Schema(s) = &parameter_data.format else {
        bail!("found unexpected 'content' data format");
    };
    let Schema::Object(obj) = &s.json_schema else {
        bail!("found unexpected `true` schema");
    };
    if obj.instance_type != Some(InstanceType::String.into()) {
        bail!("unsupported path parameter type `{:?}`", obj.instance_type);
    }

    Ok(())
}

fn response_body_schema_name(resp: ReferenceOr<openapi::Response>) -> Option<String> {
    match resp {
        ReferenceOr::Item(mut resp_body) => {
            assert!(resp_body.extensions.is_empty());
            if resp_body.content.is_empty() {
                return None;
            }

            assert_eq!(resp_body.content.len(), 1);
            let json_body = resp_body
                .content
                .swap_remove("application/json")
                .expect("should have JSON body");
            assert!(json_body.extensions.is_empty());
            match json_body.schema.expect("no json body schema?!").json_schema {
                Schema::Bool(_) => {
                    tracing::error!("unexpected bool schema");
                    None
                }
                Schema::Object(obj) => {
                    if !obj.is_ref() {
                        tracing::error!(?obj, "unexpected non-$ref json body schema");
                    }
                    get_schema_name(obj.reference)
                }
            }
        }
        ReferenceOr::Reference { .. } => {
            tracing::error!("$ref response bodies are not currently supported");
            None
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct HeaderParam {
    name: String,
    required: bool,
}

#[derive(Debug, serde::Serialize)]
struct QueryParam {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    required: bool,
    r#type: FieldType,
}
