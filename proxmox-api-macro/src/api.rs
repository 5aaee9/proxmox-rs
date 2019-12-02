use std::convert::{TryFrom, TryInto};

use failure::Error;

use proc_macro2::{Span, TokenStream};
use quote::{quote, quote_spanned};
use syn::parse::{Parse, ParseStream, Parser};
use syn::spanned::Spanned;
use syn::{ExprPath, Ident};

use crate::util::{JSONObject, JSONValue};

mod enums;
mod method;
mod structs;

/// The main `Schema` type.
///
/// We have 2 fixed keys: `type` and `description`. The remaining keys depend on the `type`.
/// Generally, we create the following mapping:
///
/// ```text
/// {
///     type: Object,
///     description: "text",
///     foo: bar, // "unknown", will be added as a builder-pattern method
///     properties: { ... }
/// }
/// ```
///
/// to:
///
/// ```text
/// {
///     ObjectSchema::new("text", &[ ... ]).foo(bar)
/// }
/// ```
struct Schema {
    span: Span,

    /// Common in all schema entry types:
    description: Option<syn::LitStr>,

    /// The specific schema type (Object, String, ...)
    item: SchemaItem,

    /// The remaining key-value pairs the `SchemaItem` parser did not extract will be appended as
    /// builder-pattern method calls to this schema.
    properties: Vec<(Ident, syn::Expr)>,
}

/// We parse this in 2 steps: first we parse a `JSONValue`, then we "parse" that further.
impl Parse for Schema {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let obj: JSONObject = input.parse()?;
        Self::try_from(obj)
    }
}

/// Shortcut:
impl TryFrom<JSONValue> for Schema {
    type Error = syn::Error;

    fn try_from(value: JSONValue) -> Result<Self, syn::Error> {
        Self::try_from(value.into_object("a schema definition")?)
    }
}

/// To go from a `JSONObject` to a `Schema` we first extract the description, as it is a common
/// element in all schema entries, then we parse the specific `SchemaItem`, and collect all the
/// remaining "unused" keys as "constraints"/"properties" which will be appended as builder-pattern
/// method calls when translating the object to a schema definition.
impl TryFrom<JSONObject> for Schema {
    type Error = syn::Error;

    fn try_from(mut obj: JSONObject) -> Result<Self, syn::Error> {
        let description = obj
            .remove("description")
            .map(|v| v.try_into())
            .transpose()?;

        Ok(Self {
            span: obj.brace_token.span,
            description,
            item: SchemaItem::try_extract_from(&mut obj)?,
            properties: obj.into_iter().try_fold(
                Vec::new(),
                |mut properties, (key, value)| -> Result<_, syn::Error> {
                    properties.push((Ident::from(key), value.try_into()?));
                    Ok(properties)
                },
            )?,
        })
    }
}

impl Schema {
    fn to_typed_schema(&self, ts: &mut TokenStream) -> Result<(), Error> {
        self.item.to_schema(
            ts,
            self.description.as_ref(),
            &self.span,
            &self.properties,
            true,
        )
    }

    fn to_schema(&self, ts: &mut TokenStream) -> Result<(), Error> {
        self.item.to_schema(
            ts,
            self.description.as_ref(),
            &self.span,
            &self.properties,
            false,
        )
    }

    fn as_object(&self) -> Option<&SchemaObject> {
        match &self.item {
            SchemaItem::Object(obj) => Some(obj),
            _ => None,
        }
    }

    fn find_object_property(&self, key: &str) -> Option<(bool, &Schema)> {
        self.as_object().and_then(|obj| obj.find_property(key))
    }
}

enum SchemaItem {
    Null,
    Boolean,
    Integer,
    String,
    Object(SchemaObject),
    Array(SchemaArray),
    ExternType(ExprPath),
}

impl SchemaItem {
    /// If there's a `type` specified, parse it as that type. Otherwise check for keys which
    /// uniqueply identify the type, such as "properties" for type `Object`.
    fn try_extract_from(obj: &mut JSONObject) -> Result<Self, syn::Error> {
        let ty = obj.remove("type").map(ExprPath::try_from).transpose()?;
        let ty = match ty {
            Some(ty) => ty,
            None => {
                if obj.contains_key("properties") {
                    return Ok(SchemaItem::Object(SchemaObject::try_extract_from(obj)?));
                } else if obj.contains_key("items") {
                    return Ok(SchemaItem::Array(SchemaArray::try_extract_from(obj)?));
                } else {
                    bail!(obj.span(), "failed to guess 'type' in schema definition");
                }
            }
        };

        if !ty.attrs.is_empty() {
            bail!(ty => "unexpected attributes on type path");
        }

        if ty.qself.is_some() || ty.path.segments.len() != 1 {
            return Ok(SchemaItem::ExternType(ty));
        }

        let name = &ty
            .path
            .segments
            .first()
            .ok_or_else(|| format_err!(&ty.path => "invalid empty path"))?
            .ident;

        const INTNAMES: &[&'static str] = &[
            "Integer", "i8", "i16", "i32", "i64", "isize", "u8", "u16", "u32", "u64", "usize",
        ];
        if name == "Null" {
            Ok(SchemaItem::Null)
        } else if name == "Boolean" || name == "bool" {
            Ok(SchemaItem::Boolean)
        } else if INTNAMES.iter().any(|n| name == n) {
            Ok(SchemaItem::Integer)
        } else if name == "String" {
            Ok(SchemaItem::String)
        } else if name == "Object" {
            Ok(SchemaItem::Object(SchemaObject::try_extract_from(obj)?))
        } else if name == "Array" {
            Ok(SchemaItem::Array(SchemaArray::try_extract_from(obj)?))
        } else {
            Ok(SchemaItem::ExternType(ty))
        }
    }

    fn to_inner_schema(
        &self,
        ts: &mut TokenStream,
        description: Option<&syn::LitStr>,
        span: &Span,
        properties: &[(Ident, syn::Expr)],
    ) -> Result<bool, Error> {
        let description = description.ok_or_else(|| format_err!(*span, "missing description"));

        match self {
            SchemaItem::Null => {
                let description = description?;
                ts.extend(quote! { ::proxmox::api::schema::NullSchema::new(#description) });
            }
            SchemaItem::Boolean => {
                let description = description?;
                ts.extend(quote! { ::proxmox::api::schema::BooleanSchema::new(#description) });
            }
            SchemaItem::Integer => {
                let description = description?;
                ts.extend(quote! { ::proxmox::api::schema::IntegerSchema::new(#description) });
            }
            SchemaItem::String => {
                let description = description?;
                ts.extend(quote! { ::proxmox::api::schema::StringSchema::new(#description) });
            }
            SchemaItem::Object(obj) => {
                let description = description?;
                let mut elems = TokenStream::new();
                obj.to_schema_inner(&mut elems)?;
                ts.extend(
                    quote! { ::proxmox::api::schema::ObjectSchema::new(#description, &[#elems]) },
                );
            }
            SchemaItem::Array(array) => {
                let description = description?;
                let mut items = TokenStream::new();
                array.to_schema(&mut items)?;
                ts.extend(quote! {
                    ::proxmox::api::schema::ArraySchema::new(#description, #items)
                });
            }
            SchemaItem::ExternType(path) => {
                if !properties.is_empty() {
                    bail!(&properties[0].0 => "additional properties not allowed on external type");
                }
                ts.extend(quote_spanned! { path.span() => #path::API_SCHEMA });
                return Ok(true);
            }
        }

        // Then append all the remaining builder-pattern properties:
        for prop in properties {
            let key = &prop.0;
            let value = &prop.1;
            ts.extend(quote! { .#key(#value) });
        }

        Ok(false)
    }

    fn to_schema(
        &self,
        ts: &mut TokenStream,
        description: Option<&syn::LitStr>,
        span: &Span,
        properties: &[(Ident, syn::Expr)],
        typed: bool,
    ) -> Result<(), Error> {
        if typed {
            let _: bool = self.to_inner_schema(ts, description, span, properties)?;
            return Ok(());
        }

        let mut inner_ts = TokenStream::new();
        if self.to_inner_schema(&mut inner_ts, description, span, properties)? {
            ts.extend(inner_ts);
        } else {
            ts.extend(quote! { & #inner_ts .schema() });
        }
        Ok(())
    }
}

/// Contains a sorted list of properties:
struct SchemaObject {
    properties: Vec<(String, bool, Schema)>,
}

impl SchemaObject {
    fn try_extract_from(obj: &mut JSONObject) -> Result<Self, syn::Error> {
        Ok(Self {
            properties: obj
                .remove_required_element("properties")?
                .into_object("object field definition")?
                .into_iter()
                .try_fold(
                    Vec::new(),
                    |mut properties, (key, value)| -> Result<_, syn::Error> {
                        let mut schema: JSONObject =
                            value.into_object("schema definition for field")?;
                        let optional: bool = schema
                            .remove("optional")
                            .map(|opt| -> Result<bool, syn::Error> {
                                let v: syn::LitBool = opt.try_into()?;
                                Ok(v.value)
                            })
                            .transpose()?
                            .unwrap_or(false);
                        properties.push((key.to_string(), optional, schema.try_into()?));
                        Ok(properties)
                    },
                )
                // This must be kept sorted!
                .map(|mut properties| {
                    properties.sort_by(|a, b| (a.0).cmp(&b.0));
                    properties
                })?,
        })
    }

    fn to_schema_inner(&self, ts: &mut TokenStream) -> Result<(), Error> {
        for element in self.properties.iter() {
            let key = &element.0;
            let optional = element.1;
            let mut schema = TokenStream::new();
            element.2.to_schema(&mut schema)?;
            ts.extend(quote! { (#key, #optional, #schema), });
        }
        Ok(())
    }

    fn find_property(&self, key: &str) -> Option<(bool, &Schema)> {
        match self
            .properties
            .binary_search_by(|prope| prope.0.as_str().cmp(key))
        {
            Ok(idx) => Some((self.properties[idx].1, &self.properties[idx].2)),
            Err(_) => None,
        }
    }
}

struct SchemaArray {
    item: Box<Schema>,
}

impl SchemaArray {
    fn try_extract_from(obj: &mut JSONObject) -> Result<Self, syn::Error> {
        Ok(Self {
            item: Box::new(obj.remove_required_element("items")?.try_into()?),
        })
    }

    fn to_schema(&self, ts: &mut TokenStream) -> Result<(), Error> {
        self.item.to_schema(ts)
    }
}

/// Parse `input`, `returns` and `protected` attributes out of an function annotated
/// with an `#[api]` attribute and produce a `const ApiMethod` named after the function.
///
/// See the top level macro documentation for a complete example.
pub(crate) fn api(attr: TokenStream, item: TokenStream) -> Result<TokenStream, Error> {
    let attribs = JSONObject::parse_inner.parse2(attr)?;
    let item: syn::Item = syn::parse2(item)?;

    match item {
        syn::Item::Fn(item) => method::handle_method(attribs, item),
        syn::Item::Struct(item) => structs::handle_struct(attribs, item),
        syn::Item::Enum(item) => enums::handle_enum(attribs, item),
        _ => bail!(item => "api macro only works on functions"),
    }
}