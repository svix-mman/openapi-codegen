use std::{borrow::Cow, collections::BTreeMap, sync::Arc};

use aide::openapi;
use anyhow::{bail, ensure, Context as _};
use schemars::schema::{InstanceType, Schema, SchemaObject, SingleOrVec};
use serde::Serialize;

use crate::util::get_schema_name;

/// Named types referenced by the [`Api`].
///
/// Intermediate representation of (some) `components` from the spec.
#[derive(Debug)]
pub(crate) struct Types(pub BTreeMap<String, Type>);

#[derive(Debug, Serialize)]
pub(crate) struct Type {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    deprecated: bool,
    #[serde(flatten)]
    data: TypeData,
}

impl Type {
    pub(crate) fn from_schema(name: String, s: SchemaObject) -> anyhow::Result<Self> {
        match s.instance_type {
            Some(SingleOrVec::Single(it)) => match *it {
                InstanceType::Object => {}
                _ => bail!("unsupported type {it:?}"),
            },
            Some(SingleOrVec::Vec(_)) => bail!("unsupported: multiple types"),
            None => bail!("unsupported: no type"),
        }

        let metadata = s.metadata.unwrap_or_default();

        let obj = s
            .object
            .context("unsupported: object type without further validation")?;

        ensure!(
            obj.additional_properties.is_none(),
            "additional_properties not yet supported"
        );
        ensure!(obj.max_properties.is_none(), "unsupported: max_properties");
        ensure!(obj.min_properties.is_none(), "unsupported: min_properties");
        ensure!(
            obj.pattern_properties.is_empty(),
            "unsupported: pattern_properties"
        );
        ensure!(obj.property_names.is_none(), "unsupported: property_names");

        Ok(Self {
            name,
            description: metadata.description,
            deprecated: metadata.deprecated,
            data: TypeData::Struct {
                fields: obj
                    .properties
                    .into_iter()
                    .map(|(name, schema)| {
                        Field::from_schema(name.clone(), schema, obj.required.contains(&name))
                            .with_context(|| format!("unsupported field {name}"))
                    })
                    .collect::<anyhow::Result<_>>()?,
            },
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum TypeData {
    Struct {
        fields: Vec<Field>,
    },
    #[allow(dead_code)] // not _yet_ supported
    Enum {
        variants: Vec<Variant>,
    },
}

#[derive(Debug, Serialize)]
pub(crate) struct Field {
    name: String,
    r#type: FieldType,
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    required: bool,
    deprecated: bool,
}

impl Field {
    fn from_schema(name: String, s: Schema, required: bool) -> anyhow::Result<Self> {
        let obj = match s {
            Schema::Bool(_) => bail!("unsupported bool schema"),
            Schema::Object(o) => o,
        };
        let metadata = obj.metadata.clone().unwrap_or_default();

        ensure!(obj.const_value.is_none(), "unsupported const_value");
        ensure!(obj.enum_values.is_none(), "unsupported enum_values");

        Ok(Self {
            name,
            r#type: FieldType::from_schema_object(obj)?,
            default: metadata.default,
            description: metadata.description,
            required,
            deprecated: metadata.deprecated,
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Variant {
    fields: Vec<Field>,
}

/// Supported field type.
///
/// Equivalent to openapi's `type` + `format` + `$ref`.
#[derive(Clone, Debug)]
pub(crate) enum FieldType {
    Bool,
    Int16,
    UInt16,
    Int32,
    Int64,
    UInt64,
    String,
    DateTime,
    Uri,
    /// A JSON object with arbitrary field values.
    JsonObject,
    /// A regular old list.
    List(Box<FieldType>),
    /// List with unique items.
    Set(Box<FieldType>),
    /// A map with a given value type.
    ///
    /// The key type is always `String` in JSON schemas.
    Map {
        value_ty: Box<FieldType>,
    },
    SchemaRef(String),
}

impl FieldType {
    pub(crate) fn from_openapi(format: openapi::ParameterSchemaOrContent) -> anyhow::Result<Self> {
        let openapi::ParameterSchemaOrContent::Schema(s) = format else {
            bail!("found unexpected 'content' data format");
        };
        Self::from_schema(s.json_schema)
    }

    fn from_schema(s: Schema) -> anyhow::Result<Self> {
        let Schema::Object(obj) = s else {
            bail!("found unexpected `true` schema");
        };

        Self::from_schema_object(obj)
    }

    fn from_schema_object(obj: SchemaObject) -> anyhow::Result<FieldType> {
        Ok(match &obj.instance_type {
            Some(SingleOrVec::Single(ty)) => match **ty {
                InstanceType::Boolean => Self::Bool,
                InstanceType::Integer => match obj.format.as_deref() {
                    Some("int16") => Self::Int16,
                    Some("uint16") => Self::UInt16,
                    Some("int32") => Self::Int32,
                    // FIXME: Why do we have int in the spec?
                    Some("int" | "int64") => Self::Int64,
                    // FIXME: Get rid of uint in the spec..
                    Some("uint" | "uint64") => Self::UInt64,
                    f => bail!("unsupported integer format: `{f:?}`"),
                },
                InstanceType::String => match obj.format.as_deref() {
                    None => Self::String,
                    Some("date-time") => Self::DateTime,
                    Some("uri") => Self::Uri,
                    Some(f) => bail!("unsupported string format: `{f:?}`"),
                },
                InstanceType::Array => {
                    let array = obj.array.context("array type must have array props")?;
                    ensure!(array.additional_items.is_none(), "not supported");
                    let inner = match array.items.context("array type must have items prop")? {
                        SingleOrVec::Single(ty) => ty,
                        SingleOrVec::Vec(types) => {
                            bail!("unsupported multi-typed array parameter: `{types:?}`")
                        }
                    };
                    let inner = Box::new(Self::from_schema(*inner)?);
                    if array.unique_items == Some(true) {
                        Self::Set(inner)
                    } else {
                        Self::List(inner)
                    }
                }
                InstanceType::Object => {
                    let obj = obj
                        .object
                        .context("unsupported: object type without further validation")?;
                    let additional_properties = obj
                        .additional_properties
                        .context("unsupported: object field type without additional_properties")?;

                    ensure!(obj.max_properties.is_none(), "unsupported: max_properties");
                    ensure!(obj.min_properties.is_none(), "unsupported: min_properties");
                    ensure!(
                        obj.properties.is_empty(),
                        "unsupported: properties on field type"
                    );
                    ensure!(
                        obj.pattern_properties.is_empty(),
                        "unsupported: pattern_properties"
                    );
                    ensure!(obj.property_names.is_none(), "unsupported: property_names");
                    ensure!(
                        obj.required.is_empty(),
                        "unsupported: required on field type"
                    );

                    match *additional_properties {
                        Schema::Bool(true) => Self::JsonObject,
                        Schema::Bool(false) => bail!("unsupported `additional_properties: false`"),
                        Schema::Object(schema_object) => {
                            let value_ty = Box::new(Self::from_schema_object(schema_object)?);
                            Self::Map { value_ty }
                        }
                    }
                }
                ty => bail!("unsupported type: `{ty:?}`"),
            },
            Some(SingleOrVec::Vec(types)) => {
                bail!("unsupported multi-typed parameter: `{types:?}`")
            }
            None => match get_schema_name(obj.reference.as_deref()) {
                Some(name) => Self::SchemaRef(name),
                None => bail!("unsupported type-less parameter"),
            },
        })
    }

    fn to_csharp_typename(&self) -> Cow<'_, str> {
        match self {
            Self::Bool => "bool".into(),
            Self::Int32 |
            // FIXME: For backwards compatibility. Should be 'long'.
            Self::Int64 | Self::UInt64 => "int".into(),
            Self::String => "string".into(),
            Self::DateTime => "DateTime".into(),
            Self::Int16 | Self::UInt16 | Self::Uri | Self::JsonObject | Self::Map { .. } => todo!(),
            // FIXME: Treat set differently?
            Self::List(field_type) | Self::Set(field_type) => {
                format!("List<{}>", field_type.to_csharp_typename()).into()
            }
            Self::SchemaRef(name) => name.clone().into(),
        }
    }

    fn to_go_typename(&self) -> Cow<'_, str> {
        match self {
            Self::Bool => "bool".into(),
            Self::Int32 |
            // FIXME: Looks like all integers are currently i32
            Self::Int64 | Self::UInt64 => "int32".into(),
            Self::String => "string".into(),
            Self::DateTime => "time.Time".into(),
            Self::Int16 | Self::UInt16 | Self::Uri | Self::JsonObject | Self::Map { .. } => todo!(),
            Self::List(field_type) | Self::Set(field_type) => {
                format!("[]{}", field_type.to_go_typename()).into()
            }
            Self::SchemaRef(name) => name.clone().into(),
        }
    }

    fn to_kotlin_typename(&self) -> Cow<'_, str> {
        match self {
            Self::Bool => "Boolean".into(),
            Self::Int32 |
            // FIXME: Should be Long..
            Self::Int64 | Self::UInt64 => "Int".into(),
            Self::String => "String".into(),
            Self::DateTime => "OffsetDateTime".into(),
            Self::Int16 | Self::UInt16 | Self::Uri | Self::JsonObject | Self::Map { .. } => todo!(),
            // FIXME: Treat set differently?
            Self::List(field_type) | Self::Set(field_type) => {
                format!("List<{}>", field_type.to_kotlin_typename()).into()
            }
            Self::SchemaRef(name) => name.clone().into(),
        }
    }

    fn to_js_typename(&self) -> Cow<'_, str> {
        match self {
            Self::Bool => "boolean".into(),
            Self::Int16 | Self::UInt16 | Self::Int32 | Self::Int64 | Self::UInt64 => {
                "number".into()
            }
            Self::String => "string".into(),
            Self::DateTime => "Date | null".into(),
            Self::Uri | Self::JsonObject | Self::Map { .. } => todo!(),
            Self::List(field_type) | Self::Set(field_type) => {
                format!("{}[]", field_type.to_js_typename()).into()
            }
            Self::SchemaRef(name) => name.clone().into(),
        }
    }

    fn to_rust_typename(&self) -> Cow<'_, str> {
        match self {
            Self::Bool => "bool".into(),
            Self::Int16 => "i16".into(),
            Self::UInt16 => "u16".into(),
            Self::Int32 |
            // FIXME: All integers in query params are currently i32
            Self::Int64 | Self::UInt64 => "i32".into(),
            // FIXME: Do we want a separate type for Uri?
            Self::Uri | Self::String => "String".into(),
            // FIXME: Depends on those chrono imports being in scope, not that great..
            Self::DateTime => "DateTime<Utc>".into(),
            Self::JsonObject => "serde_json::Value".into(),
            // FIXME: Treat set differently? (BTreeSet)
            Self::List(field_type) | Self::Set(field_type) => {
                format!("Vec<{}>", field_type.to_rust_typename()).into()
            }
            Self::Map { value_ty } => format!(
                "std::collections::HashMap<String, {}>",
                value_ty.to_rust_typename(),
            )
            .into(),
            Self::SchemaRef(name) => name.clone().into(),
        }
    }
}

impl minijinja::value::Object for FieldType {
    fn repr(self: &Arc<Self>) -> minijinja::value::ObjectRepr {
        minijinja::value::ObjectRepr::Plain
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &minijinja::State<'_, '_>,
        method: &str,
        args: &[minijinja::Value],
    ) -> Result<minijinja::Value, minijinja::Error> {
        match method {
            "to_csharp" => {
                ensure_no_args(args, "to_csharp")?;
                Ok(self.to_csharp_typename().into())
            }
            "to_go" => {
                ensure_no_args(args, "to_go")?;
                Ok(self.to_go_typename().into())
            }
            "to_js" => {
                ensure_no_args(args, "to_js")?;
                Ok(self.to_js_typename().into())
            }
            "to_kotlin" => {
                ensure_no_args(args, "to_kotlin")?;
                Ok(self.to_kotlin_typename().into())
            }
            "to_rust" => {
                ensure_no_args(args, "to_rust")?;
                Ok(self.to_rust_typename().into())
            }
            "is_datetime" => {
                ensure_no_args(args, "is_datetime")?;
                Ok(matches!(**self, Self::DateTime).into())
            }
            _ => Err(minijinja::Error::from(minijinja::ErrorKind::UnknownMethod)),
        }
    }
}

fn ensure_no_args(args: &[minijinja::Value], method_name: &str) -> Result<(), minijinja::Error> {
    if !args.is_empty() {
        return Err(minijinja::Error::new(
            minijinja::ErrorKind::TooManyArguments,
            format!("{method_name} does not take any arguments"),
        ));
    }
    Ok(())
}

impl serde::Serialize for FieldType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        minijinja::Value::from_object(self.clone()).serialize(serializer)
    }
}
