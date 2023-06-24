use std::{collections::HashMap};

use anyhow::anyhow;
use arroyo_datastream::SerializationMode;
use arroyo_rpc::grpc::{
    self,
    api::{ConnectionSchema, TestSourceMessage, TableType},
};
use http::SSEConnector;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::sync::mpsc::Sender;
use tonic::Status;
use typify::import_types;

use self::kafka::KafkaConnector;

pub mod http;
pub mod kafka;

import_types!(schema = "../connector-schemas/common.json",);

#[derive(Debug, Copy, Clone)]
pub enum ConnectionType {
    Source,
    Sink,
}

#[derive(Debug, Clone)]
pub struct Connection {
    pub id: Option<i64>,
    pub name: String,
    pub connection_type: ConnectionType,
    pub schema: ConnectionSchema,
    pub operator: String,
    pub config: String,
    pub description: String,
}

pub trait Connector: Send {
    type ConfigT: DeserializeOwned + Serialize;
    type TableT: DeserializeOwned + Serialize;

    fn name(&self) -> &'static str;

    fn parse_config(&self, s: &str) -> Result<Self::ConfigT, serde_json::Error> {
        serde_json::from_str(s)
    }

    fn parse_table(&self, s: &str) -> Result<Self::TableT, serde_json::Error> {
        serde_json::from_str(s)
    }

    fn metadata(&self) -> grpc::api::Connector;

    fn table_type(&self, config: Self::ConfigT, table: Self::TableT) -> TableType;

    fn test(
        &self,
        name: &str,
        config: Self::ConfigT,
        table: Self::TableT,
        schema: Option<&ConnectionSchema>,
        tx: Sender<Result<TestSourceMessage, Status>>,
    );

    fn from_options(&self, name: &str, options: &mut HashMap<String, String>,
        schema: Option<&ConnectionSchema>) -> anyhow::Result<Connection>;

    fn from_config(
        &self,
        id: Option<i64>,
        name: &str,
        config: Self::ConfigT,
        table: Self::TableT,
        schema: Option<&ConnectionSchema>,
    ) -> anyhow::Result<Connection>;
}

pub trait ErasedConnector: Send {
    fn name(&self) -> &'static str;

    fn metadata(&self) -> grpc::api::Connector;

    fn validate_config(&self, s: &str) -> Result<(), serde_json::Error>;

    fn validate_table(&self, s: &str) -> Result<(), serde_json::Error>;

    fn table_type(&self, config: &str, table: &str) -> Result<TableType, serde_json::Error>;

    fn test(
        &self,
        name: &str,
        config: &str,
        table: &str,
        schema: Option<&ConnectionSchema>,
        tx: Sender<Result<TestSourceMessage, Status>>,
    ) -> Result<(), serde_json::Error>;

    fn from_options(&self, name: &str, options: &mut HashMap<String, String>,
        schema: Option<&ConnectionSchema>) -> anyhow::Result<Connection>;

    fn from_config(
        &self,
        id: Option<i64>,
        name: &str,
        config: &str,
        table: &str,
        schema: Option<&ConnectionSchema>,
    ) -> anyhow::Result<Connection>;
}

impl<C: Connector> ErasedConnector for C {
    fn name(&self) -> &'static str {
        self.name()
    }

    fn metadata(&self) -> grpc::api::Connector {
        self.metadata()
    }

    fn validate_config(&self, s: &str) -> Result<(), serde_json::Error> {
        self.parse_config(s).map(|_| ())
    }

    fn validate_table(&self, s: &str) -> Result<(), serde_json::Error> {
        self.parse_table(s).map(|_| ())
    }

    fn table_type(&self, config: &str, table: &str) -> Result<TableType, serde_json::Error> {
        Ok(self.table_type(self.parse_config(config)?, self.parse_table(table)?))
    }

    fn test(
        &self,
        name: &str,
        config: &str,
        table: &str,
        schema: Option<&ConnectionSchema>,
        tx: Sender<Result<TestSourceMessage, Status>>,
    ) -> Result<(), serde_json::Error> {
        self.test(
            name,
            self.parse_config(config)?,
            self.parse_table(table)?,
            schema,
            tx,
        );

        Ok(())
    }

    fn from_options(&self, name: &str, options: &mut HashMap<String, String>,
        schema: Option<&ConnectionSchema>) -> anyhow::Result<Connection> {
            self.from_options(name, options, schema)
        }

    fn from_config(
        &self,
        id: Option<i64>,
        name: &str,
        config: &str,
        table: &str,
        schema: Option<&ConnectionSchema>,
    ) -> anyhow::Result<Connection> {
        self.from_config(
            id,
            name,
            self.parse_config(config)?,
            self.parse_table(table)?,
            schema,
        )
    }
}

pub(crate) fn pull_opt(name: &str, opts: &mut HashMap<String, String>) -> anyhow::Result<String> {
    opts.remove(name)
        .ok_or_else(|| anyhow!("Required option '{}' not set", name))
}

pub fn connector_for_type(t: &str) -> Option<Box<dyn ErasedConnector>> {
    connectors().remove(t)
}

pub fn connectors() -> HashMap<&'static str, Box<dyn ErasedConnector>> {
    let mut m: HashMap<&'static str, Box<dyn ErasedConnector>> = HashMap::new();
    m.insert("kafka", Box::new(KafkaConnector {}));
    m.insert("sse", Box::new(SSEConnector {}));

    m
}

pub fn serialization_mode(schema: &ConnectionSchema) -> OperatorConfigSerializationMode {
    let confluent = schema
        .format_options
        .as_ref()
        .filter(|t| t.confluent_schema_registry)
        .is_some();
    match &schema.format() {
        grpc::api::Format::JsonFormat => {
            if confluent {
                OperatorConfigSerializationMode::JsonSchemaRegistry
            } else {
                OperatorConfigSerializationMode::Json
            }
        }
        grpc::api::Format::ProtobufFormat => todo!(),
        grpc::api::Format::AvroFormat => todo!(),
        grpc::api::Format::RawStringFormat => {
            if confluent {
                todo!("support raw json schemas with confluent schema registry decoding")
            } else {
                OperatorConfigSerializationMode::RawJson
            }
        }
        grpc::api::Format::DebeziumJsonFormat => {
            OperatorConfigSerializationMode::DebeziumJson
        },
    }
}

impl From<OperatorConfigSerializationMode> for SerializationMode {
    fn from(value: OperatorConfigSerializationMode) -> Self {
        match value {
            OperatorConfigSerializationMode::Json => SerializationMode::Json,
            OperatorConfigSerializationMode::JsonSchemaRegistry => {
                SerializationMode::JsonSchemaRegistry
            }
            OperatorConfigSerializationMode::RawJson => SerializationMode::RawJson,
            OperatorConfigSerializationMode::DebeziumJson => SerializationMode::DebeziumJson,
        }
    }
}
