use std::str::FromStr;

use anyhow::Context;
use arroyo_connectors::connector_for_type;
use axum::extract::{Path, State};
use axum::Json;
use cornucopia_async::GenericClient;
use deadpool_postgres::Transaction;
use http::StatusCode;
use prost::Message;
use serde_json::Value;
use tonic::Status;
use tracing::warn;

use arroyo_datastream::{ConnectorOp, Operator, Program};
use arroyo_rpc::grpc::api::{
    self, create_pipeline_req, CreatePipelineReq, CreateSqlJob, PipelineDef, PipelineGraphReq,
    PipelineGraphResp, PipelineProgram, SqlError, SqlErrors, Udf, UdfLanguage,
};
use arroyo_rpc::public_ids::{generate_id, IdTypes};
use arroyo_sql::{ArroyoSchemaProvider, SqlConfig};
use arroyo_types::api::{JobCollection, Pipeline, PipelineCollection, PipelinePost};

use crate::connection_tables;
use crate::queries::api_queries;
use crate::queries::api_queries::{DbPipeline, DbPipelineRest};
use crate::rest::AppState;
use crate::rest_utils::{authenticate, client, log_and_map_rest, BearerAuth, ErrorResp};
use crate::types::public::PipelineType;
use crate::{handle_db_error, log_and_map, optimizations, required_field, AuthData};

async fn compile_sql<'e, E>(
    sql: &CreateSqlJob,
    auth_data: &AuthData,
    tx: &E,
) -> Result<(Program, Vec<i64>), Status>
where
    E: GenericClient,
{
    let mut schema_provider = ArroyoSchemaProvider::new();

    for (i, udf) in sql.udfs.iter().enumerate() {
        match UdfLanguage::from_i32(udf.language) {
            Some(UdfLanguage::Rust) => {
                schema_provider.add_rust_udf(&udf.definition).map_err(|e| {
                    Status::invalid_argument(format!("Could not process UDF: {:?}", e))
                })?;
            }
            None => {
                return Err(required_field(&format!("udfs[{}].language", i)));
            }
        }
    }

    for table in connection_tables::get(auth_data, tx).await? {
        let Some(connector) = connector_for_type(&table.connector) else {
            warn!("Saved table found with unknown connector {}", table.connector);
            continue;
        };

        let connection = connector
            .from_config(
                Some(table.id),
                &table.name,
                &table
                    .connection
                    .map(|c| c.config.clone())
                    .unwrap_or_else(|| "{}".to_string()),
                &table.config,
                table.schema.as_ref(),
            )
            .map_err(log_and_map)?;

        schema_provider.add_connector_table(connection);
    }

    let (program, connections) = arroyo_sql::parse_and_get_program(
        &sql.query,
        schema_provider,
        SqlConfig {
            default_parallelism: sql.parallelism as usize,
        },
    )
    .await
    .with_context(|| "failed to generate SQL program")
    .map_err(|err| {
        warn!("{:?}", err);
        Status::invalid_argument(format!("{}", err.root_cause()))
    })?;

    Ok((program, connections))
}

fn set_parallelism(program: &mut Program, parallelism: usize) {
    for node in program.graph.node_weights_mut() {
        node.parallelism = parallelism;
    }
}

pub(crate) async fn create_pipeline<'a>(
    req: CreatePipelineReq,
    auth: AuthData,
    tx: &Transaction<'a>,
) -> Result<i64, Status> {
    let pipeline_type;
    let mut program;
    let connections;
    let text;
    let udfs: Option<Vec<Udf>>;
    let is_preview;

    match req.config.ok_or_else(|| required_field("config"))? {
        create_pipeline_req::Config::Program(bytes) => {
            if !auth.org_metadata.can_create_programs {
                return Err(Status::invalid_argument(
                    "Your plan does not allow you to call this API.",
                ));
            }
            pipeline_type = PipelineType::rust;
            program = PipelineProgram::decode(&bytes[..])
                .map_err(log_and_map)?
                .try_into()
                .map_err(log_and_map)?;
            connections = vec![];
            text = None;
            udfs = None;
            is_preview = false;
        }
        create_pipeline_req::Config::Sql(sql) => {
            if sql.parallelism > auth.org_metadata.max_parallelism as u64 {
                return Err(Status::invalid_argument(format!(
                    "Your plan allows you to run pipelines up to parallelism {};
                    contact support@arroyo.systems for an increase",
                    auth.org_metadata.max_parallelism
                )));
            }

            pipeline_type = PipelineType::sql;
            (program, connections) = compile_sql(&sql, &auth, tx).await?;
            text = Some(sql.query);
            udfs = Some(
                sql.udfs
                    .iter()
                    .map(|t| Udf {
                        language: t.language,
                        definition: t.definition.clone(),
                    })
                    .collect(),
            );
            is_preview = sql.preview;
        }
    };

    optimizations::optimize(&mut program.graph);

    if program.graph.node_count() > auth.org_metadata.max_operators as usize {
        return Err(Status::invalid_argument(
            format!("This pipeline is too large to create under your plan, which only allows pipelines up to {} nodes;
                contact support@arroyo.systems for an increase", auth.org_metadata.max_operators)));
    }

    let errors = program.validate_graph();
    if !errors.is_empty() {
        let errs: Vec<String> = errors.iter().map(|s| format!("  * {}\n", s)).collect();

        return Err(Status::failed_precondition(format!(
            "Program validation failed:\n{}",
            errs.join("")
        )));
    }

    set_parallelism(&mut program, 1);

    if is_preview {
        for node in program.graph.node_weights_mut() {
            // if it is a connector sink or switch to a web sink
            if let Operator::ConnectorSink { .. } = node.operator {
                node.operator = Operator::ConnectorSink(ConnectorOp::web_sink());
            }
        }
    }

    let proto_program: PipelineProgram = program.try_into().map_err(log_and_map)?;

    let program = proto_program.encode_to_vec();
    let version = 2;

    if req.name.is_empty() {
        return Err(required_field("name"));
    }

    let pipeline_id = api_queries::create_pipeline()
        .bind(
            tx,
            &generate_id(IdTypes::Pipeline),
            &auth.organization_id,
            &auth.user_id,
            &req.name,
            &pipeline_type,
            &version,
        )
        .one()
        .await
        .map_err(|e| handle_db_error("pipeline", e))?;

    api_queries::create_pipeline_definition()
        .bind(
            tx,
            &generate_id(IdTypes::PipelineDefinition),
            &auth.organization_id,
            &auth.user_id,
            &pipeline_id,
            &version,
            &text,
            &udfs.map(|t| serde_json::to_value(t).unwrap()),
            &program,
        )
        .one()
        .await
        .map_err(log_and_map)?;

    if !is_preview {
        for connection in connections {
            api_queries::add_pipeline_connection_table()
                .bind(
                    tx,
                    &generate_id(IdTypes::ConnectionTablePipeline),
                    &pipeline_id,
                    &connection,
                )
                .await
                .map_err(log_and_map)?;
        }
    }

    Ok(pipeline_id)
}

impl TryInto<PipelineDef> for DbPipeline {
    type Error = Status;

    fn try_into(self) -> Result<PipelineDef, Self::Error> {
        let program: Program = PipelineProgram::decode(&self.program[..])
            .map_err(log_and_map)?
            .try_into()
            .map_err(log_and_map)?;

        Ok(PipelineDef {
            pipeline_id: format!("{}", self.id),
            name: self.name,
            r#type: format!("{:?}", self.r#type),
            definition: self.textual_repr,
            udfs: serde_json::from_value(self.udfs.unwrap_or_else(|| Value::Array(vec![])))
                .map_err(log_and_map)?,
            job_graph: Some(program.as_job_graph()),
        })
    }
}

impl Into<Pipeline> for DbPipelineRest {
    fn into(self) -> Pipeline {
        Pipeline {
            id: self.id,
            name: self.name,
            definition: self.textual_repr,
            udfs: vec![], //todo
        }
    }
}

pub(crate) async fn query_pipeline_rest(
    id: &str,
    auth: &AuthData,
    db: &impl GenericClient,
) -> Result<Pipeline, ErrorResp> {
    let pipeline = api_queries::get_pipeline_rest()
        .bind(db, &id, &auth.organization_id)
        .opt()
        .await
        .map_err(log_and_map_rest)?;

    let res: DbPipelineRest = pipeline.ok_or_else(|| ErrorResp {
        status_code: StatusCode::NOT_FOUND,
        message: "Pipeline not found".to_string(),
    })?;

    Ok(res.into())
}

pub(crate) async fn query_pipeline(
    id: &str,
    auth: &AuthData,
    db: &impl GenericClient,
) -> Result<PipelineDef, Status> {
    if let Ok(id) = i64::from_str(id) {
        let res = api_queries::get_pipeline()
            .bind(db, &id, &auth.organization_id)
            .opt()
            .await
            .map_err(log_and_map)?;

        if let Some(res) = res {
            return res.try_into();
        }
    }

    Err(Status::not_found(format!("No pipeline with id {}", id)))
}

pub(crate) async fn sql_graph(
    req: PipelineGraphReq,
    auth: AuthData,
    client: &impl GenericClient,
) -> Result<PipelineGraphResp, Status> {
    let sql = CreateSqlJob {
        query: req.query,
        parallelism: 1,
        udfs: req.udfs,
        preview: false,
    };

    match compile_sql(&sql, &auth, client).await {
        Ok((mut program, _)) => {
            optimizations::optimize(&mut program.graph);
            Ok(PipelineGraphResp {
                result: Some(api::pipeline_graph_resp::Result::JobGraph(
                    program.as_job_graph(),
                )),
            })
        }
        Err(err) => match err.code() {
            tonic::Code::InvalidArgument => Ok(PipelineGraphResp {
                result: Some(api::pipeline_graph_resp::Result::Errors(SqlErrors {
                    errors: vec![SqlError {
                        message: err.message().to_string(),
                    }],
                })),
            }),
            _ => Err(err),
        },
    }
}

#[utoipa::path(
    post,
    path = "/v1/pipelines",
    tag = "pipelines",
    request_body = PipelinePost,
    responses(
        (status = 200, description = "Created pipeline", body = Pipeline),
    ),
)]
pub async fn post_pipeline(
    State(state): State<AppState>,
    bearer_auth: BearerAuth,
    Json(pipelinePost): Json<PipelinePost>,
) -> Result<Json<Pipeline>, ErrorResp> {
    let client = client(&state.pool).await?;
    let auth_data = authenticate(&state.pool, bearer_auth).await?;
    Err(ErrorResp {
        status_code: StatusCode::NOT_IMPLEMENTED,
        message: "Method not implemented yet".to_string(),
    })
}

#[utoipa::path(
    get,
    path = "/v1/pipelines",
    tag = "pipelines",
    responses(
        (status = 200, description = "Got pipelines collection", body = PipelineCollection),
    ),
)]
pub async fn get_pipelines(
    State(state): State<AppState>,
    bearer_auth: BearerAuth,
) -> Result<Json<PipelineCollection>, ErrorResp> {
    let client = client(&state.pool).await?;
    let auth_data = authenticate(&state.pool, bearer_auth).await?;
    Err(ErrorResp {
        status_code: StatusCode::NOT_IMPLEMENTED,
        message: "Method not implemented yet".to_string(),
    })
}

#[utoipa::path(
    get,
    path = "/v1/pipelines/{id}",
    tag = "pipelines",
    responses(
        (status = 200, description = "Got pipeline", body = Pipeline),
    ),
)]
pub async fn get_pipeline(
    State(state): State<AppState>,
    bearer_auth: BearerAuth,
    Path(id): Path<String>,
) -> Result<Json<Pipeline>, ErrorResp> {
    let client = client(&state.pool).await?;
    let auth_data = authenticate(&state.pool, bearer_auth).await?;

    let p = query_pipeline_rest(&id, &auth_data, &client).await?;
    Ok(Json(p))

    // Err(ErrorResp {
    //     status_code: StatusCode::NOT_IMPLEMENTED,
    //     message: "Method not implemented yet".to_string(),
    // })
}

#[utoipa::path(
    get,
    path = "/v1/pipelines/{id}/jobs",
    tag = "pipelines",
    responses(
        (status = 200, description = "Got jobs collection", body = JobCollection),
    ),
)]
pub async fn get_jobs(
    State(state): State<AppState>,
    bearer_auth: BearerAuth,
) -> Result<Json<JobCollection>, ErrorResp> {
    let client = client(&state.pool).await?;
    let auth_data = authenticate(&state.pool, bearer_auth).await?;
    Err(ErrorResp {
        status_code: StatusCode::NOT_IMPLEMENTED,
        message: "Method not implemented yet".to_string(),
    })
}
