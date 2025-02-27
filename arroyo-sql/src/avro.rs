use crate::types::{StructDef, StructField, TypeDef};
use anyhow::{anyhow, bail};
use apache_avro::Schema;
use arrow_schema::{DataType, TimeUnit};
use arroyo_rpc::formats::AvroFormat;
use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};

pub const ROOT_NAME: &str = "ArroyoAvroRoot";

pub fn convert_avro_schema(name: &str, schema: &str) -> anyhow::Result<Vec<StructField>> {
    convert_avro_schema_helper(name, schema, false)
}

fn convert_avro_schema_helper(
    name: &str,
    schema: &str,
    for_generation: bool,
) -> anyhow::Result<Vec<StructField>> {
    let schema =
        Schema::parse_str(schema).map_err(|e| anyhow!("avro schema is not valid: {:?}", e))?;

    let (typedef, _) = to_typedef(name, &schema, for_generation);
    match typedef {
        TypeDef::StructDef(sd, _) => Ok(sd.fields),
        TypeDef::DataType(_, _) => {
            bail!("top-level schema must be a record")
        }
    }
}

pub fn get_defs(name: &str, schema: &str) -> anyhow::Result<String> {
    let fields = convert_avro_schema_helper(name, schema, true)?;

    let sd = StructDef::new(Some(ROOT_NAME.to_string()), true, fields, None);
    let defs: Vec<_> = sd
        .all_structs_including_named()
        .iter()
        .map(|p| {
            vec![
                syn::parse_str(&p.def(false)).unwrap(),
                p.generate_serializer_items(),
            ]
        })
        .flatten()
        .collect();

    let mod_ident: Ident = syn::parse_str(name).unwrap();
    Ok(quote! {
        mod #mod_ident {
            use super::*;
            #(#defs)
            *
        }
    }
    .to_string())
}

fn to_typedef(
    source_name: &str,
    schema: &Schema,
    for_generation: bool,
) -> (TypeDef, Option<String>) {
    match schema {
        Schema::Null => (TypeDef::DataType(DataType::Null, false), None),
        Schema::Boolean => (TypeDef::DataType(DataType::Boolean, false), None),
        Schema::Int | Schema::TimeMillis => (TypeDef::DataType(DataType::Int32, false), None),
        Schema::Long
        | Schema::TimeMicros
        | Schema::TimestampMillis
        | Schema::LocalTimestampMillis
        | Schema::LocalTimestampMicros => (TypeDef::DataType(DataType::Int64, false), None),
        Schema::Float => (TypeDef::DataType(DataType::Float32, false), None),
        Schema::Double => (TypeDef::DataType(DataType::Float64, false), None),
        Schema::Bytes | Schema::Fixed(_) | Schema::Decimal(_) => {
            (TypeDef::DataType(DataType::Binary, false), None)
        }
        Schema::String | Schema::Enum(_) | Schema::Uuid => {
            (TypeDef::DataType(DataType::Utf8, false), None)
        }
        Schema::Union(union) => {
            // currently just support unions that have [t, null] as variants, which is the
            // avro way to represent optional fields

            let (nulls, not_nulls): (Vec<_>, Vec<_>) = union
                .variants()
                .iter()
                .partition(|v| matches!(v, Schema::Null));

            if nulls.len() == 1 && not_nulls.len() == 1 {
                let (dt, original) = to_typedef(source_name, not_nulls[0], for_generation);
                (dt.to_optional(), original)
            } else {
                (
                    TypeDef::DataType(DataType::Utf8, false),
                    Some("json".to_string()),
                )
            }
        }
        Schema::Record(record) => {
            let fields = record
                .fields
                .iter()
                .map(|f| {
                    let (ft, original) = to_typedef(source_name, &f.schema, for_generation);
                    StructField::with_rename(f.name.clone(), None, ft, None, original)
                })
                .collect();

            let name = if for_generation {
                // if we're generating the actual structs, we don't want to namespace
                record.name.name.to_string()
            } else {
                // otherwise, if we're getting the schema, we need the namespacing to find the proper structs
                format!("{}::{}", source_name, record.name.name)
            };

            (
                TypeDef::StructDef(StructDef::for_name(Some(name), fields), false),
                None,
            )
        }
        _ => (
            TypeDef::DataType(DataType::Utf8, false),
            Some("json".to_string()),
        ),
    }
}

/// Generates code that serializes an arroyo data struct into avro
///
/// Note that this must align with the schemas constructed in
/// `arroyo-formats::avro::arrow_to_avro_schema`!
pub fn generate_serializer_item(
    record: &Ident,
    field: Option<&Ident>,
    name: Option<String>,
    td: &TypeDef,
) -> TokenStream {
    use DataType::*;
    let value = match field {
        Some(ident) => quote!(#record.#ident),
        None => quote!(#record),
    };

    let (inner, nullable) = match td {
        TypeDef::StructDef(def, nullable) => {
            let fields: Vec<_> = def
                .fields
                .iter()
                .map(|f| {
                    let name = AvroFormat::sanitize_field(&match f.alias.as_ref() {
                        Some(alias) => format!("{}.{}", alias, f.name),
                        None => f.name(),
                    });
                    let field_ident = f.field_ident();
                    let serializer = generate_serializer_item(
                        &format_ident!("v"),
                        Some(&field_ident),
                        Some(name.clone()),
                        &f.data_type,
                    );

                    quote! {
                        __avro_record.put(#name, #serializer);
                    }
                })
                .collect();

            let schema_extractor = name.map(|name| {
                let nullable_handler = if *nullable {
                    Some(quote! {
                        let arroyo_worker::apache_avro::Schema::Union(__union_schema) = schema else {
                            panic!("invalid avro schema -- struct field {} is nullable and should be represented by a union", #name);
                        };

                        let schema = __union_schema.variants().get(1)
                          .expect("invalid avro schema -- struct field {} should be a union with two variants");
                    })
                } else {
                    None
                };

                quote! {
                    let arroyo_worker::apache_avro::Schema::Record(__record_schema) = schema else {
                        panic!("invalid avro schema -- struct field {} should correspond to record schema", #name);
                    };

                    let __record_field_number = __record_schema.lookup.get(#name).unwrap();
                    let schema = &__record_schema.fields[*__record_field_number].schema;

                    #nullable_handler
                }
            });

            (
                quote!({
                    #schema_extractor

                    let mut __avro_record = arroyo_worker::apache_avro::types::Record::new(schema).unwrap();

                    #(#fields )*

                    Into::<arroyo_worker::apache_avro::types::Value>::into(__avro_record)
                }),
                *nullable,
            )
        }
        TypeDef::DataType(dt, nullable) => (
            match dt {
                Null => unreachable!("null fields are not supported"),
                Boolean => quote! { Boolean(*v) },
                Int8 | Int16 | Int32 | UInt8 | UInt16 => quote! { Int(*v as i32) },
                Int64 | UInt32 | UInt64 => quote! { Long(*v as i64) },
                Float16 | Float32 => quote! { Float(*v as f32) },
                Float64 => quote! { Double(*v as f64) },
                Timestamp(t, tz) => match (t, tz) {
                    (TimeUnit::Microsecond | TimeUnit::Nanosecond, None) => {
                        quote! { TimestampMicros(arroyo_types::to_micros(*v) as i64) }
                    }
                    (TimeUnit::Microsecond | TimeUnit::Nanosecond, Some(_)) => {
                        quote! { LocalTimestampMicros(arroyo_types::to_micros(*v) as i64) }
                    }
                    (TimeUnit::Millisecond | TimeUnit::Second, None) => {
                        quote! { Long(arroyo_types::to_millis(*v) as i64) }
                    }
                    (TimeUnit::Millisecond | TimeUnit::Second, Some(_)) => {
                        quote! { LocalTimestampMillis(arroyo_types::to_millis(*v) as i64) }
                    }
                },
                Date32 | Date64 => quote! { Date(arroyo_types::days_since_epoch(*v)) },
                Time32(_) => todo!("time32 is not supported"),
                Time64(_) => todo!("time64 is not supported"),
                Duration(_) => todo!("duration is not supported"),
                Interval(_) => todo!("interval is not supported"),
                Binary | FixedSizeBinary(_) | LargeBinary => quote! { Bytes(v.clone()) },
                Utf8 | LargeUtf8 => quote! { String(v.clone()) },
                List(_) | FixedSizeList(_, _) | LargeList(_) => {
                    todo!("lists are not supported")
                }
                Struct(_) => unreachable!("typedefs should not contain structs"),
                Union(_, _) => unimplemented!("unions are not supported"),
                Dictionary(_, _) => unimplemented!("dictionaries are not supported"),
                Decimal128(_, _) => unimplemented!("decimal128 is not supported"),
                Decimal256(_, _) => unimplemented!("decimal256 is not supported"),
                Map(_, _) => unimplemented!("maps are not supported"),
                RunEndEncoded(_, _) => unimplemented!("run end encoded is not supported"),
            },
            *nullable,
        ),
    };

    if nullable {
        quote! {
            Union(#value.is_some() as u32, Box::new(#value.as_ref().map(|v| #inner).unwrap_or(Null)))
        }
    } else {
        quote! {{let v = &#value; #inner}}
    }
}
