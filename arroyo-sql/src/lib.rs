#![allow(clippy::new_without_default)]
use anyhow::{anyhow, bail, Result};
use arrow::array::ArrayRef;
use arrow::datatypes::{self, DataType, Field};
use arrow_schema::TimeUnit;
use arroyo_connectors::kafka::{KafkaConfig, KafkaConnector, KafkaTable};
use arroyo_connectors::{Connection, Connector};
use arroyo_datastream::Program;

use datafusion::physical_plan::functions::make_scalar_function;
pub mod avro;
pub(crate) mod code_gen;
pub mod expressions;
pub mod external;
pub mod json_schema;
mod operators;
mod optimizations;
mod pipeline;
mod plan_graph;
pub mod schemas;
mod tables;
pub mod types;

use datafusion::prelude::create_udf;

use datafusion::sql::sqlparser::dialect::PostgreSqlDialect;
use datafusion::sql::sqlparser::parser::Parser;
use datafusion::sql::{planner::ContextProvider, TableReference};

use datafusion_expr::{
    logical_plan::builder::LogicalTableSource, AggregateUDF, ScalarUDF, TableSource,
};
use datafusion_expr::{
    AccumulatorFactoryFunction, LogicalPlan, ReturnTypeFunction, Signature, StateTypeFunction,
    Volatility, WindowUDF,
};
use expressions::{Expression, ExpressionContext};
use pipeline::{SqlOperator, SqlPipelineBuilder};
use plan_graph::{get_program, PlanGraph};
use schemas::window_arrow_struct;
use tables::{schema_defs, ConnectorTable, Insert, Table};

use crate::code_gen::{CodeGenerator, ValuePointerContext};
use crate::types::{StructDef, StructField, TypeDef};
use arroyo_rpc::api_types::connections::{ConnectionProfile, ConnectionSchema, ConnectionType};
use arroyo_rpc::formats::{Format, JsonFormat};
use datafusion_common::DataFusionError;
use prettyplease::unparse;
use regex::Regex;
use std::collections::HashSet;

use arroyo_rpc::{OperatorConfig, UdfOpts};
use std::time::{Duration, SystemTime};
use std::{collections::HashMap, sync::Arc};
use syn::{parse_file, parse_quote, parse_str, FnArg, Item, ReturnType, Visibility};
use toml::Value;
use tracing::warn;
use unicase::UniCase;

const DEFAULT_IDLE_TIME: Option<Duration> = Some(Duration::from_secs(5 * 60));

#[cfg(test)]
mod test;

#[derive(Clone, Debug)]
pub struct UdfDef {
    args: Vec<TypeDef>,
    ret: TypeDef,
    def: String,
    dependencies: String,
    opts: UdfOpts,
    async_fn: bool,
    has_context: bool,
}

#[derive(Clone, Debug)]
pub struct CompiledSql {
    pub program: Program,
    pub connection_ids: Vec<i64>,
    pub schemas: HashMap<String, StructDef>,
}

#[derive(Debug, Clone, Default)]
pub struct ArroyoSchemaProvider {
    pub source_defs: HashMap<String, String>,
    tables: HashMap<UniCase<String>, Table>,
    pub functions: HashMap<String, Arc<ScalarUDF>>,
    pub aggregate_functions: HashMap<String, Arc<AggregateUDF>>,
    pub connections: HashMap<String, Connection>,
    profiles: HashMap<String, ConnectionProfile>,
    pub udf_defs: HashMap<String, UdfDef>,
    config_options: datafusion::config::ConfigOptions,
}

impl ArroyoSchemaProvider {
    pub fn new() -> Self {
        let tables = HashMap::new();
        let mut functions = HashMap::new();

        let fn_impl = |args: &[ArrayRef]| Ok(Arc::new(args[0].clone()) as ArrayRef);

        let window_return_type = Arc::new(window_arrow_struct());
        functions.insert(
            "hop".to_string(),
            Arc::new(create_udf(
                "hop",
                vec![
                    DataType::Interval(datatypes::IntervalUnit::MonthDayNano),
                    DataType::Interval(datatypes::IntervalUnit::MonthDayNano),
                ],
                window_return_type.clone(),
                Volatility::Volatile,
                make_scalar_function(fn_impl),
            )),
        );
        functions.insert(
            "tumble".to_string(),
            Arc::new(create_udf(
                "tumble",
                vec![DataType::Interval(datatypes::IntervalUnit::MonthDayNano)],
                window_return_type.clone(),
                Volatility::Volatile,
                make_scalar_function(fn_impl),
            )),
        );
        functions.insert(
            "session".to_string(),
            Arc::new(create_udf(
                "session",
                vec![DataType::Interval(datatypes::IntervalUnit::MonthDayNano)],
                window_return_type,
                Volatility::Volatile,
                make_scalar_function(fn_impl),
            )),
        );
        functions.insert(
            "unnest".to_string(),
            Arc::new({
                let return_type: ReturnTypeFunction = Arc::new(move |args| {
                    match args.get(0).ok_or_else(|| {
                        DataFusionError::Plan("unnest takes one argument".to_string())
                    })? {
                        DataType::List(t) => Ok(Arc::new(t.data_type().clone())),
                        _ => Err(DataFusionError::Plan(
                            "unnest may only be called on arrays".to_string(),
                        )),
                    }
                });
                ScalarUDF::new(
                    "unnest",
                    &Signature::any(1, Volatility::Immutable),
                    &return_type,
                    &make_scalar_function(fn_impl),
                )
            }),
        );
        functions.insert(
            "get_first_json_object".to_string(),
            Arc::new(create_udf(
                "get_first_json_object",
                vec![DataType::Utf8, DataType::Utf8],
                Arc::new(DataType::Utf8),
                Volatility::Volatile,
                make_scalar_function(fn_impl),
            )),
        );

        functions.insert(
            "get_json_objects".to_string(),
            Arc::new(create_udf(
                "get_json_objects",
                vec![DataType::Utf8, DataType::Utf8],
                Arc::new(DataType::List(Arc::new(Field::new(
                    "item",
                    DataType::Utf8,
                    false,
                )))),
                Volatility::Volatile,
                make_scalar_function(fn_impl),
            )),
        );
        functions.insert(
            "extract_json".to_string(),
            Arc::new(create_udf(
                "extract_json",
                vec![DataType::Utf8, DataType::Utf8],
                Arc::new(DataType::List(Arc::new(Field::new(
                    "item",
                    DataType::Utf8,
                    false,
                )))),
                Volatility::Volatile,
                make_scalar_function(fn_impl),
            )),
        );

        functions.insert(
            "extract_json_string".to_string(),
            Arc::new(create_udf(
                "extract_json_string",
                vec![DataType::Utf8, DataType::Utf8],
                Arc::new(DataType::Utf8),
                Volatility::Volatile,
                make_scalar_function(fn_impl),
            )),
        );

        Self {
            tables,
            functions,
            aggregate_functions: HashMap::new(),
            source_defs: HashMap::new(),
            connections: HashMap::new(),
            profiles: HashMap::new(),
            udf_defs: HashMap::new(),
            config_options: datafusion::config::ConfigOptions::new(),
        }
    }

    pub fn add_connector_table(&mut self, connection: Connection) {
        if let Some(def) = schema_defs(&connection.name, &connection.schema) {
            self.source_defs.insert(connection.name.clone(), def);
        }

        self.tables.insert(
            UniCase::new(connection.name.clone()),
            Table::ConnectorTable(connection.into()),
        );
    }

    pub fn add_connection_profile(&mut self, profile: ConnectionProfile) {
        self.profiles.insert(profile.name.clone(), profile);
    }

    fn insert_table(&mut self, table: Table) {
        self.tables
            .insert(UniCase::new(table.name().to_string()), table);
    }

    pub fn get_table(&self, table_name: impl Into<String>) -> Option<&Table> {
        self.tables.get(&UniCase::new(table_name.into()))
    }

    pub fn get_table_mut(&mut self, table_name: impl Into<String>) -> Option<&mut Table> {
        self.tables.get_mut(&UniCase::new(table_name.into()))
    }

    fn vec_inner_type(ty: &syn::Type) -> Option<syn::Type> {
        if let syn::Type::Path(syn::TypePath { path, .. }) = ty {
            if let Some(segment) = path.segments.last() {
                if segment.ident == "Vec" {
                    if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                        if args.args.len() == 1 {
                            if let syn::GenericArgument::Type(inner_ty) = &args.args[0] {
                                return Some(inner_ty.clone());
                            }
                        }
                    }
                }
            }
        }
        None
    }

    pub fn add_rust_udf(&mut self, body: &str) -> Result<String> {
        let mut file = parse_file(body)?;

        let mut functions = file.items.iter_mut().filter_map(|item| match item {
            Item::Fn(function) => Some(function),
            _ => None,
        });

        let function = match (functions.next(), functions.next()) {
            (Some(function), None) => function,
            _ => bail!("UDF definition must contain exactly 1 function."),
        };

        let name = function.sig.ident.to_string();
        let async_fn = function.sig.asyncness.is_some();
        let mut args: Vec<TypeDef> = vec![];
        let mut vec_arguments = 0;

        let inputs = function.sig.inputs.iter();
        let mut skip = 0;
        let mut has_context = false;

        if async_fn {
            // skip the first argument if it is a context
            if function.sig.inputs.len() >= 1 {
                if let FnArg::Typed(t) = function.sig.inputs.first().unwrap() {
                    if let syn::Pat::Ident(i) = &*t.pat {
                        if i.ident == "context" {
                            // TODO: how to ensure type is Arc<Context>?
                            has_context = true;
                            skip = 1
                        }
                    }
                }
            }
        }

        for (i, arg) in inputs.skip(skip).enumerate() {
            match arg {
                FnArg::Receiver(_) => {
                    bail!(
                        "Function {} has a 'self' argument, which is not allowed",
                        name
                    )
                }
                FnArg::Typed(t) => {
                    if let Some(vec_type) = Self::vec_inner_type(&*t.ty) {
                        vec_arguments += 1;
                        args.push((&vec_type).try_into().map_err(|_| {
                                anyhow!(
                                    "Could not convert function {} inner vector arg {} into a SQL data type",
                                    name,
                                    i
                                )
                            })?);
                    } else {
                        args.push((&*t.ty).try_into().map_err(|_| {
                            anyhow!(
                                "Could not convert function {} arg {} into a SQL data type",
                                name,
                                i
                            )
                        })?);
                    }
                }
            }
        }

        let ret: TypeDef = match &function.sig.output {
            ReturnType::Default => bail!("Function {} return type must be specified", name),
            ReturnType::Type(_, t) => (&**t).try_into().map_err(|_| {
                anyhow!(
                    "Could not convert function {} return type into a SQL data type",
                    name
                )
            })?,
        };
        if vec_arguments > 0 && vec_arguments != args.len() {
            bail!("Function {} arguments must be vectors or none", name);
        }
        if vec_arguments > 0 {
            let return_type = Arc::new(ret.as_datatype().unwrap().clone());
            let name = function.sig.ident.to_string();
            let signature = Signature::exact(
                args.iter()
                    .map(|t| t.as_datatype().unwrap().clone())
                    .collect(),
                Volatility::Volatile,
            );
            let return_type: ReturnTypeFunction = Arc::new(move |_| Ok(return_type.clone()));
            let accumulator: AccumulatorFactoryFunction = Arc::new(|_| unreachable!());
            let state_type: StateTypeFunction = Arc::new(|_| unreachable!());
            let udaf =
                AggregateUDF::new(&name, &signature, &return_type, &accumulator, &state_type);
            self.aggregate_functions
                .insert(function.sig.ident.to_string(), Arc::new(udaf));
        } else {
            let fn_impl = |args: &[ArrayRef]| Ok(Arc::new(args[0].clone()) as ArrayRef);

            if self
                .functions
                .insert(
                    function.sig.ident.to_string(),
                    Arc::new(create_udf(
                        &function.sig.ident.to_string(),
                        args.iter()
                            .map(|t| t.as_datatype().unwrap().clone())
                            .collect(),
                        Arc::new(ret.as_datatype().unwrap().clone()),
                        Volatility::Volatile,
                        make_scalar_function(fn_impl),
                    )),
                )
                .is_some()
            {
                warn!(
                    "Global UDF '{}' is being overwritten",
                    function.sig.ident.to_string()
                );
            };
        }

        function.vis = Visibility::Public(Default::default());

        self.udf_defs.insert(
            function.sig.ident.to_string(),
            UdfDef {
                args,
                ret,
                async_fn,
                def: unparse(&file.clone()),
                dependencies: parse_dependencies(&body)?,
                opts: parse_udf_opts(&body)?,
                has_context,
            },
        );

        Ok(name)
    }
}

fn get_toml_value(definition: &str) -> Result<Option<Value>> {
    // find block comments that contain toml
    let re = Regex::new(r"\/\*\n(.*\n[\s\S]*?)\*\/").unwrap();
    let mut toml_comment = None;

    for captures in re.captures_iter(&definition) {
        let val = captures.get(1).unwrap().as_str();
        if let Ok(t) = toml::from_str::<Value>(val) {
            if toml_comment.is_some() {
                bail!("Only one configuration comment is allowed in a UDF");
            }
            toml_comment = Some(t);
        }
    }

    Ok(toml_comment)
}

pub fn parse_dependencies(definition: &str) -> Result<String> {
    // ensure 1 valid toml comment
    get_toml_value(definition)?;

    // get content of dependencies comment using regex
    let re = Regex::new(r"(?m)\*\n[\s\S]*?(\[dependencies\]\n[\s\S]*?)(?:^$|\*/)").unwrap();

    return if let Some(captures) = re.captures(&definition) {
        if captures.len() != 2 {
            bail!("Error parsing dependencies");
        }
        Ok(captures.get(1).unwrap().as_str().to_string())
    } else {
        Ok("[dependencies]\n# not defined\n".to_string())
    };
}

pub fn parse_udf_opts(definition: &str) -> Result<UdfOpts> {
    if let Some(t) = get_toml_value(definition)? {
        if let Some(opts) = t.get("udfs") {
            let u: UdfOpts = opts.clone().try_into()?;
            return Ok(u);
        }
    }
    Ok(serde_json::from_str("{}")?) // default
}

fn create_table_source(fields: Vec<Field>) -> Arc<dyn TableSource> {
    Arc::new(LogicalTableSource::new(Arc::new(
        datatypes::Schema::new_with_metadata(fields, HashMap::new()),
    )))
}

impl ContextProvider for ArroyoSchemaProvider {
    fn get_table_provider(
        &self,
        name: TableReference,
    ) -> datafusion_common::Result<Arc<dyn TableSource>> {
        let table = self.get_table(name.to_string()).ok_or_else(|| {
            datafusion::error::DataFusionError::Plan(format!("Table {} not found", name))
        })?;

        let fields = table.get_fields();

        Ok(create_table_source(fields))
    }

    fn get_function_meta(&self, name: &str) -> Option<Arc<ScalarUDF>> {
        self.functions.get(name).cloned()
    }

    fn get_aggregate_meta(&self, name: &str) -> Option<Arc<AggregateUDF>> {
        self.aggregate_functions.get(name).cloned()
    }

    fn get_variable_type(&self, _variable_names: &[String]) -> Option<DataType> {
        None
    }

    fn options(&self) -> &datafusion::config::ConfigOptions {
        &self.config_options
    }

    fn get_window_meta(&self, _name: &str) -> Option<Arc<WindowUDF>> {
        None
    }
}

#[derive(Clone, Debug)]
pub struct SqlConfig {
    pub default_parallelism: usize,
}

impl Default for SqlConfig {
    fn default() -> Self {
        Self {
            default_parallelism: 4,
        }
    }
}

pub async fn parse_and_get_program(
    query: &str,
    schema_provider: ArroyoSchemaProvider,
    config: SqlConfig,
) -> Result<CompiledSql> {
    let query = query.to_string();

    if query.trim().is_empty() {
        bail!("Query is empty");
    }

    tokio::spawn(async move { parse_and_get_program_sync(query, schema_provider, config) })
        .await
        .map_err(|_| anyhow!("Something went wrong"))?
}

pub fn parse_and_get_program_sync(
    query: String,
    mut schema_provider: ArroyoSchemaProvider,
    config: SqlConfig,
) -> Result<CompiledSql> {
    let dialect = PostgreSqlDialect {};
    let mut inserts = vec![];
    for statement in Parser::parse_sql(&dialect, &query)? {
        if let Some(table) = Table::try_from_statement(&statement, &schema_provider)? {
            schema_provider.insert_table(table);
        } else {
            inserts.push(Insert::try_from_statement(
                &statement,
                &mut schema_provider,
            )?);
        };
    }

    let mut sql_pipeline_builder = SqlPipelineBuilder::new(&mut schema_provider);
    for insert in inserts {
        sql_pipeline_builder.add_insert(insert)?;
    }

    let mut plan_graph = PlanGraph::new(config.clone());

    // if there are no insert nodes, return an error
    if sql_pipeline_builder.insert_nodes.is_empty() {
        bail!("The provided SQL does not contain a query");
    }

    // If there isn't a sink, add a web sink to the last insert
    if !sql_pipeline_builder
        .insert_nodes
        .iter()
        .any(|n| matches!(n, SqlOperator::Sink(..)))
    {
        let insert = sql_pipeline_builder.insert_nodes.pop().unwrap();
        let struct_def = insert.return_type();
        let sink = Table::ConnectorTable(ConnectorTable {
            id: None,
            name: "web".to_string(),
            connection_type: ConnectionType::Sink,
            fields: struct_def.fields.into_iter().map(|f| f.into()).collect(),
            type_name: None,
            operator: "GrpcSink::<#in_k, #in_t>".to_string(),
            config: serde_json::to_string(&OperatorConfig::default()).unwrap(),
            description: "WebSink".to_string(),
            format: Some(Format::Json(JsonFormat {
                debezium: insert.is_updating(),
                ..Default::default()
            })),
            event_time_field: None,
            watermark_field: None,
            idle_time: DEFAULT_IDLE_TIME,
            inferred_fields: None,
        });

        plan_graph.add_sql_operator(sink.as_sql_sink(insert)?);
    }

    for output in sql_pipeline_builder.insert_nodes.into_iter() {
        plan_graph.add_sql_operator(output);
    }

    get_program(plan_graph, sql_pipeline_builder.schema_provider.clone())
}

#[derive(Clone)]
pub struct TestStruct {
    pub non_nullable_i32: i32,
    pub nullable_i32: Option<i32>,
    pub non_nullable_bool: bool,
    pub nullable_bool: Option<bool>,
    pub non_nullable_f32: f32,
    pub nullable_f32: Option<f32>,
    pub non_nullable_f64: f64,
    pub nullable_f64: Option<f64>,
    pub non_nullable_i64: i64,
    pub nullable_i64: Option<i64>,
    pub non_nullable_string: String,
    pub nullable_string: Option<String>,
    pub non_nullable_timestamp: SystemTime,
    pub nullable_timestamp: Option<SystemTime>,
    pub non_nullable_bytes: Vec<u8>,
    pub nullable_bytes: Option<Vec<u8>>,
}

impl Default for TestStruct {
    fn default() -> Self {
        Self {
            non_nullable_i32: Default::default(),
            nullable_i32: Default::default(),
            non_nullable_bool: Default::default(),
            nullable_bool: Default::default(),
            non_nullable_f32: Default::default(),
            nullable_f32: Default::default(),
            non_nullable_f64: Default::default(),
            nullable_f64: Default::default(),
            non_nullable_i64: Default::default(),
            nullable_i64: Default::default(),
            non_nullable_string: Default::default(),
            nullable_string: Default::default(),
            non_nullable_timestamp: SystemTime::UNIX_EPOCH,
            nullable_timestamp: None,
            non_nullable_bytes: Default::default(),
            nullable_bytes: Default::default(),
        }
    }
}

fn test_struct_def() -> StructDef {
    StructDef::for_name(
        Some("TestStruct".to_string()),
        vec![
            StructField::new(
                "non_nullable_i32".to_string(),
                None,
                TypeDef::DataType(DataType::Int32, false),
            ),
            StructField::new(
                "nullable_i32".to_string(),
                None,
                TypeDef::DataType(DataType::Int32, true),
            ),
            StructField::new(
                "non_nullable_bool".to_string(),
                None,
                TypeDef::DataType(DataType::Boolean, false),
            ),
            StructField::new(
                "nullable_bool".to_string(),
                None,
                TypeDef::DataType(DataType::Boolean, true),
            ),
            StructField::new(
                "non_nullable_f32".to_string(),
                None,
                TypeDef::DataType(DataType::Float32, false),
            ),
            StructField::new(
                "nullable_f32".to_string(),
                None,
                TypeDef::DataType(DataType::Float32, true),
            ),
            StructField::new(
                "non_nullable_f64".to_string(),
                None,
                TypeDef::DataType(DataType::Float64, false),
            ),
            StructField::new(
                "nullable_f64".to_string(),
                None,
                TypeDef::DataType(DataType::Float64, true),
            ),
            StructField::new(
                "non_nullable_i64".to_string(),
                None,
                TypeDef::DataType(DataType::Int64, false),
            ),
            StructField::new(
                "nullable_i64".to_string(),
                None,
                TypeDef::DataType(DataType::Int64, true),
            ),
            StructField::new(
                "non_nullable_string".to_string(),
                None,
                TypeDef::DataType(DataType::Utf8, false),
            ),
            StructField::new(
                "nullable_string".to_string(),
                None,
                TypeDef::DataType(DataType::Utf8, true),
            ),
            StructField::new(
                "non_nullable_timestamp".to_string(),
                None,
                TypeDef::DataType(DataType::Timestamp(TimeUnit::Microsecond, None), false),
            ),
            StructField::new(
                "nullable_timestamp".to_string(),
                None,
                TypeDef::DataType(DataType::Timestamp(TimeUnit::Microsecond, None), true),
            ),
            StructField::new(
                "non_nullable_bytes".to_string(),
                None,
                TypeDef::DataType(DataType::Binary, false),
            ),
            StructField::new(
                "nullable_bytes".to_string(),
                None,
                TypeDef::DataType(DataType::Binary, true),
            ),
        ],
    )
}

pub fn generate_test_code(
    function_suffix: &str,
    generating_expression: &Expression,
    struct_tokens: &syn::Expr,
    result_expression: &syn::Expr,
) -> syn::ItemFn {
    let syn_expr = generating_expression.generate(&ValuePointerContext::new());
    let function_name: syn::Ident =
        parse_str(&format!("generated_test_{}", function_suffix)).unwrap();
    parse_quote!(
                fn #function_name() {
                    assert_eq!({let arg = #struct_tokens;#syn_expr}, #result_expression);
    })
}

pub fn get_test_expression(
    test_name: &str,
    calculation_string: &str,
    input_value: &syn::Expr,
    expected_result: &syn::Expr,
) -> syn::ItemFn {
    let struct_def = test_struct_def();
    let schema = ConnectionSchema {
        format: Some(Format::Json(JsonFormat::default())),
        bad_data: None,
        framing: None,
        struct_name: struct_def.name.clone(),
        fields: struct_def
            .fields
            .iter()
            .map(|s| s.clone().try_into().unwrap())
            .collect(),
        definition: None,
        inferred: None,
    };

    let mut schema_provider = ArroyoSchemaProvider::new();
    let kafka = (KafkaConnector {})
        .from_config(
            Some(1),
            "test_source",
            KafkaConfig {
                authentication: arroyo_connectors::kafka::KafkaConfigAuthentication::None {},
                bootstrap_servers: "localhost:9092".to_string().try_into().unwrap(),
                schema_registry_enum: None,
            },
            KafkaTable {
                topic: "test_topic".to_string(),
                type_: arroyo_connectors::kafka::TableType::Source {
                    offset: arroyo_connectors::kafka::SourceOffset::Latest,
                    read_mode: Some(arroyo_connectors::kafka::ReadMode::ReadUncommitted),
                    group_id: "test-consumer-group".to_string().try_into().unwrap(),
                },
                client_configs: HashMap::new(),
            },
            Some(&schema),
        )
        .unwrap();

    schema_provider.add_connector_table(kafka);

    let mut inserts = vec![];
    for statement in Parser::parse_sql(
        &PostgreSqlDialect {},
        &format!("SELECT {} FROM test_source", calculation_string),
    )
    .unwrap()
    {
        if let Some(table) = Table::try_from_statement(&statement, &schema_provider).unwrap() {
            schema_provider.insert_table(table);
        } else {
            inserts.push(Insert::try_from_statement(&statement, &mut schema_provider).unwrap());
        };
    }

    let Insert::Anonymous {
        logical_plan: LogicalPlan::Projection(projection),
    } = inserts.remove(0)
    else {
        panic!("expect projection")
    };
    let ctx = ExpressionContext {
        schema_provider: &schema_provider,
        input_struct: &struct_def,
    };

    let generating_expression = ctx.compile_expr(&projection.expr[0]).unwrap();

    generate_test_code(
        test_name,
        &generating_expression,
        input_value,
        expected_result,
    )
}

pub fn has_duplicate_udf_names<'a>(definitions: impl Iterator<Item = &'a String>) -> bool {
    let mut udf_names = HashSet::new();
    for definition in definitions {
        let Ok(file) = syn::parse_file(definition) else {
            warn!("Could not parse UDF definition: {}", definition);
            continue;
        };

        for item in file.items {
            let Item::Fn(function) = item else {
                continue;
            };

            if udf_names.contains(&function.sig.ident.to_string()) {
                return true;
            }

            udf_names.insert(function.sig.ident.to_string());
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dependencies_valid() {
        let definition = r#"
/*
[dependencies]
serde = "1.0"
*/

pub fn my_udf() -> i64 {
    1
}
        "#;

        assert_eq!(
            parse_dependencies(definition).unwrap(),
            r#"[dependencies]
serde = "1.0"
"#
        );
    }

    #[test]
    fn test_parse_dependencies_valid_with_udfs() {
        let definition = r#"
/*
[dependencies]
serde = "1.0"

[udfs]
async_results_ordered = true

*/

pub fn my_udf() -> i64 {
    1
}
        "#;

        assert_eq!(
            parse_dependencies(definition).unwrap(),
            r#"[dependencies]
serde = "1.0"
"#
        );
    }

    #[test]
    fn test_parse_dependencies_none() {
        let definition = r#"
pub fn my_udf() -> i64 {
    1
}
        "#;

        assert_eq!(
            parse_dependencies(definition).unwrap(),
            r#"[dependencies]
# not defined
"#
        );
    }

    #[test]
    fn test_parse_dependencies_multiple() {
        let definition = r#"
/*
[dependencies]
serde = "1.0"
*/

/*
[dependencies]
serde = "1.0"
*/

pub fn my_udf() -> i64 {
    1
}
        "#;
        assert!(parse_dependencies(definition).is_err());
    }

    #[test]
    fn test_parse_multiple_toml() {
        let definition = r#"
/*
[dependencies]
serde = "1.0"
*/

/*
[udfs]
async_results_ordered = true
*/

pub fn my_udf() -> i64 {
    1
}
        "#;
        assert!(parse_dependencies(definition).is_err());
    }

    #[test]
    fn test_parse_udf_ops_ordered_true() {
        let input = r#"
/*
[dependencies]
serde = "1.0"

[udfs]
async_results_ordered = true
*/

pub fn my_udf() -> i64 {
    1
}
        "#;

        let opts = parse_udf_opts(input).unwrap();

        assert_eq!(opts.async_results_ordered, true);
    }

    #[test]
    fn test_parse_udf_ops_ordered_false() {
        let input = r#"
/*
[dependencies]
serde = "1.0"

[udfs]
async_results_ordered = false
*/

pub fn my_udf() -> i64 {
    1
}
        "#;

        let opts = parse_udf_opts(input).unwrap();

        assert_eq!(opts.async_results_ordered, false);
    }
}
