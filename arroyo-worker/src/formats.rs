use std::{collections::HashMap, marker::PhantomData};

use arrow::datatypes::{Field, Fields};
use arroyo_types::{
    formats::{Format, JsonFormat},
    UserError,
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};

use crate::SchemaData;

fn deserialize_slice_json<T: DeserializeOwned>(
    format: &JsonFormat,
    msg: &[u8],
) -> Result<T, String> {
    let msg = if format.confluent_schema_registry {
        &msg[5..]
    } else {
        msg
    };

    if format.unstructured {
        let j = if format.include_schema {
            // we need to deserialize it to pull out the payload
            let v: Value = serde_json::from_slice(&msg)
                .map_err(|e| format!("Failed to deserialize json: {:?}", e))?;
            let payload = v.get("payload").ok_or_else(|| {
                "`include_schema` set to true, but record does not have a payload field".to_string()
            })?;

            json! {
                { "value": serde_json::to_string(payload).unwrap() }
            }
        } else {
            json! {
                { "value": String::from_utf8_lossy(msg) }
            }
        };

        // TODO: this is inefficient, because we know that T is RawJson in this case and can much more directly
        //  produce that value. However, without specialization I don't know how to get the compiler to emit
        //  the optimized code for that case.
        Ok(serde_json::from_value(j).unwrap())
    } else {
        serde_json::from_slice(msg)
            .map_err(|e| format!("Failed to deserialize JSON into schema: {:?}", e))
    }
}

pub fn deserialize_slice<T: DeserializeOwned>(format: &Format, msg: &[u8]) -> Result<T, UserError> {
    match format {
        Format::Json(json) => deserialize_slice_json(json, msg),
        Format::Avro(_) => todo!(),
        Format::Parquet(_) => todo!(),
        Format::Raw(_) => todo!(),
    }
    .map_err(|e| {
        UserError::new(
            "Deserialization failed",
            format!(
                "Failed to deserialize: '{}': {}",
                String::from_utf8_lossy(&msg),
                e
            ),
        )
    })
}

pub struct DataSerializer<T: SchemaData + Serialize> {
    kafka_schema: Value,
    #[allow(unused)]
    json_schema: Value,
    format: Format,
    _t: PhantomData<T>,
}

impl<T: SchemaData + Serialize> DataSerializer<T> {
    pub fn new(format: Format) -> Self {
        Self {
            kafka_schema: arrow_to_kafka_json(T::name(), T::schema().fields()),
            json_schema: arrow_to_json_schema(T::schema().fields()),
            format,
            _t: PhantomData,
        }
    }

    pub fn to_vec(&self, record: &T) -> Vec<u8> {
        match &self.format {
            Format::Json(json) => {
                let v = if json.include_schema {
                    let record = json! {{
                        "schema": self.kafka_schema,
                        "payload": record
                    }};

                    serde_json::to_vec(&record).unwrap()
                } else {
                    serde_json::to_vec(record).unwrap()
                };

                if json.confluent_schema_registry {
                    todo!("Serializing to confluent schema registry is not yet supported");
                }

                v
            }
            Format::Avro(_) => todo!(),
            Format::Parquet(_) => todo!(),
            Format::Raw(_) => todo!(),
        }
    }
}

#[derive(Debug)]
pub struct MilliSecondsSystemTimeVisitor;

// Custom datetime serializer for use with Debezium JSON
pub mod timestamp_as_millis {
    use std::{fmt, time::SystemTime};

    use arroyo_types::{from_millis, to_millis};
    use serde::{de, Deserializer, Serializer};

    use super::MilliSecondsSystemTimeVisitor;

    pub fn serialize<S>(t: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(to_millis(*t))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_i64(MilliSecondsSystemTimeVisitor)
    }

    impl<'de> de::Visitor<'de> for MilliSecondsSystemTimeVisitor {
        type Value = SystemTime;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a unix timestamp in milliseconds")
        }

        /// Deserialize a timestamp in milliseconds since the epoch
        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if value >= 0 {
                Ok(from_millis(value as u64))
            } else {
                Err(serde::de::Error::custom("millis must be positive"))
            }
        }

        /// Deserialize a timestamp in milliseconds since the epoch
        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(from_millis(value))
        }
    }
}

fn field_to_json_schema(field: &Field) -> Value {
    match field.data_type() {
        arrow::datatypes::DataType::Null => {
            json! {{ "type": "null" }}
        }
        arrow::datatypes::DataType::Boolean => {
            json! {{ "type": "boolean" }}
        }
        arrow::datatypes::DataType::Int8
        | arrow::datatypes::DataType::Int16
        | arrow::datatypes::DataType::Int32
        | arrow::datatypes::DataType::Int64
        | arrow::datatypes::DataType::UInt8
        | arrow::datatypes::DataType::UInt16
        | arrow::datatypes::DataType::UInt32
        | arrow::datatypes::DataType::UInt64 => {
            // TODO: integer bounds
            json! {{ "type": "integer" }}
        }
        arrow::datatypes::DataType::Float16
        | arrow::datatypes::DataType::Float32
        | arrow::datatypes::DataType::Float64 => {
            json! {{ "type": "number" }}
        }
        arrow::datatypes::DataType::Timestamp(_, _) => {
            json! {{ "type": "string", "format": "date-time" }}
        }
        arrow::datatypes::DataType::Date32
        | arrow::datatypes::DataType::Date64
        | arrow::datatypes::DataType::Time32(_)
        | arrow::datatypes::DataType::Time64(_) => {
            todo!()
        }
        arrow::datatypes::DataType::Duration(_) => todo!(),
        arrow::datatypes::DataType::Interval(_) => todo!(),
        arrow::datatypes::DataType::Binary
        | arrow::datatypes::DataType::FixedSizeBinary(_)
        | arrow::datatypes::DataType::LargeBinary => {
            json! {{ "type": "array", "items": { "type": "integer" }}}
        }
        arrow::datatypes::DataType::Utf8 | arrow::datatypes::DataType::LargeUtf8 => {
            json! {{ "type": "string" }}
        }
        arrow::datatypes::DataType::List(t)
        | arrow::datatypes::DataType::FixedSizeList(t, _)
        | arrow::datatypes::DataType::LargeList(t) => {
            json! {{"type": "array", "items": field_to_json_schema(&*t) }}
        }
        arrow::datatypes::DataType::Struct(s) => arrow_to_json_schema(s),
        arrow::datatypes::DataType::Union(_, _) => todo!(),
        arrow::datatypes::DataType::Dictionary(_, _) => todo!(),
        arrow::datatypes::DataType::Decimal128(_, _) => todo!(),
        arrow::datatypes::DataType::Decimal256(_, _) => todo!(),
        arrow::datatypes::DataType::Map(_, _) => todo!(),
        arrow::datatypes::DataType::RunEndEncoded(_, _) => todo!(),
    }
}

fn arrow_to_json_schema(fields: &Fields) -> Value {
    let props: HashMap<String, Value> = fields
        .iter()
        .map(|f| (f.name().clone(), field_to_json_schema(f)))
        .collect();

    let required: Vec<String> = fields
        .iter()
        .filter(|f| !f.is_nullable())
        .map(|f| f.name().clone())
        .collect();
    json! {{
        "type": "object",
        "properties": props,
        "required": required,
    }}
}

fn field_to_kafka_json(field: &Field) -> Value {
    use arrow::datatypes::DataType::*;

    let typ = match field.data_type() {
        Null => todo!(),
        Boolean => "boolean",
        Int8 | UInt8 => "int8",
        Int16 | UInt16 => "uint16",
        Int32 | UInt32 => "uint32",
        Int64 | UInt64 => "uint64",
        Float16 | Float32 => "float",
        Float64 => "double",
        Utf8 | LargeUtf8 => "string",
        Binary | FixedSizeBinary(_) | LargeBinary => "bytes",
        Time32(_) | Time64(_) | Timestamp(_, _) => {
            // as far as I can tell, this is the only way to get timestamps from Arroyo into
            // Kafka connect
            return json! {{
                "type": "int64",
                "field": field.name().clone(),
                "optional": field.is_nullable(),
                "name": "org.apache.kafka.connect.data.Timestamp"
            }};
        }
        Date32 | Date64 => {
            return json! {{
                "type": "int64",
                "field": field.name().clone(),
                "optional": field.is_nullable(),
                "name": "org.apache.kafka.connect.data.Date"
            }}
        }
        Duration(_) => "int64",
        Interval(_) => "int64",
        List(t) | FixedSizeList(t, _) | LargeList(t) => {
            return json! {{
                "type": "array",
                "items": field_to_kafka_json(&*t),
                "field": field.name().clone(),
                "optional": field.is_nullable(),
            }};
        }
        Struct(s) => {
            let fields: Vec<_> = s.iter().map(|f| field_to_kafka_json(&*f)).collect();
            return json! {{
                "type": "struct",
                "fields": fields,
                "field": field.name().clone(),
                "optional": field.is_nullable(),
            }};
        }
        Union(_, _) => todo!(),
        Dictionary(_, _) => todo!(),
        Decimal128(_, _) => todo!(),
        Decimal256(_, _) => todo!(),
        Map(_, _) => todo!(),
        RunEndEncoded(_, _) => todo!(),
    };

    json! {{
        "type": typ,
        "field": field.name().clone(),
        "optional": field.is_nullable(),
    }}
}

// For some reason Kafka uses it's own bespoke almost-but-not-quite JSON schema format
// https://www.confluent.io/blog/kafka-connect-deep-dive-converters-serialization-explained/#json-schemas
fn arrow_to_kafka_json(name: &str, fields: &Fields) -> Value {
    let fields: Vec<_> = fields.iter().map(|f| field_to_kafka_json(&*f)).collect();
    json! {{
        "type": "struct",
        "name": name,
        "fields": fields,
        "optional": false,
    }}
}
