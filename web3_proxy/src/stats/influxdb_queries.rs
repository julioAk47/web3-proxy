use super::StatType;
use crate::frontend::errors::Web3ProxyErrorContext;
use crate::{
    app::Web3ProxyApp,
    frontend::errors::{Web3ProxyError, Web3ProxyResponse},
    http_params::{
        get_chain_id_from_params, get_query_start_from_params, get_query_stop_from_params,
        get_query_window_seconds_from_params,
    },
};
use anyhow::Context;
use axum::{
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Json, TypedHeader,
};
use entities::sea_orm_active_enums::Role;
use entities::{rpc_key, secondary_user};
use fstrings::{f, format_args_f};
use hashbrown::HashMap;
use influxdb2::api::query::FluxRecord;
use influxdb2::models::Query;
use log::{error, info, warn};
use migration::sea_orm::ColumnTrait;
use migration::sea_orm::EntityTrait;
use migration::sea_orm::QueryFilter;
use serde_json::json;
use ulid::Ulid;

pub async fn query_user_stats<'a>(
    app: &'a Web3ProxyApp,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    params: &'a HashMap<String, String>,
    stat_response_type: StatType,
) -> Web3ProxyResponse {
    let user_id = match bearer {
        Some(inner_bearer) => {
            let (user, _semaphore) = app.bearer_is_authorized(inner_bearer.0 .0).await?;
            user.id
        }
        None => 0,
    };

    // Return an error if the bearer is set, but the StatType is Detailed
    if stat_response_type == StatType::Detailed && user_id == 0 {
        return Err(Web3ProxyError::BadRequest(
            "Detailed Stats Response requires you to authorize with a bearer token".to_owned(),
        ));
    }

    let db_replica = app
        .db_replica()
        .context("query_user_stats needs a db replica")?;

    // TODO: have a getter for this. do we need a connection pool on it?
    let influxdb_client = app
        .influxdb_client
        .as_ref()
        .context("query_user_stats needs an influxdb client")?;

    let query_window_seconds = get_query_window_seconds_from_params(params)?;
    let query_start = get_query_start_from_params(params)?.timestamp();
    let query_stop = get_query_stop_from_params(params)?.timestamp();
    let chain_id = get_chain_id_from_params(app, params)?;

    // Return a bad request if query_start == query_stop, because then the query is empty basically
    if query_start == query_stop {
        return Err(Web3ProxyError::BadRequest(
            "Start and Stop date cannot be equal. Please specify a (different) start date."
                .to_owned(),
        ));
    }

    let measurement = if user_id == 0 {
        "global_proxy"
    } else {
        "opt_in_proxy"
    };

    // Include a hashmap to go from rpc_secret_key_id to the rpc_secret_key
    let mut rpc_key_id_to_key = HashMap::new();

    let rpc_key_filter = if user_id == 0 {
        "".to_string()
    } else {
        // Fetch all rpc_secret_key_ids, and filter for these
        let mut user_rpc_keys = rpc_key::Entity::find()
            .filter(rpc_key::Column::UserId.eq(user_id))
            .all(db_replica.conn())
            .await
            .web3_context("failed loading user's key")?
            .into_iter()
            .map(|x| {
                let key = x.id.to_string();
                let val = Ulid::from(x.secret_key);
                rpc_key_id_to_key.insert(key.clone(), val);
                key
            })
            .collect::<Vec<_>>();

        // Fetch all rpc_keys where we are the subuser
        let mut subuser_rpc_keys = secondary_user::Entity::find()
            .filter(secondary_user::Column::UserId.eq(user_id))
            .find_also_related(rpc_key::Entity)
            .all(db_replica.conn())
            // TODO: Do a join with rpc-keys
            .await
            .web3_context("failed loading subuser keys")?
            .into_iter()
            .flat_map(
                |(subuser, wrapped_shared_rpc_key)| match wrapped_shared_rpc_key {
                    Some(shared_rpc_key) => {
                        if subuser.role == Role::Admin || subuser.role == Role::Owner {
                            let key = shared_rpc_key.id.to_string();
                            let val = Ulid::from(shared_rpc_key.secret_key);
                            rpc_key_id_to_key.insert(key.clone(), val);
                            Some(key)
                        } else {
                            None
                        }
                    }
                    None => None,
                },
            )
            .collect::<Vec<_>>();

        user_rpc_keys.append(&mut subuser_rpc_keys);

        if user_rpc_keys.is_empty() {
            return Err(Web3ProxyError::BadRequest(
                "User has no secret RPC keys yet".to_string(),
            ));
        }

        // Iterate, pop and add to string
        f!(
            r#"|> filter(fn: (r) => contains(value: r["rpc_secret_key_id"], set: {:?}))"#,
            user_rpc_keys
        )
    };

    // TODO: Turn into a 500 error if bucket is not found ..
    // Or just unwrap or so
    let bucket = &app
        .config
        .influxdb_bucket
        .clone()
        .context("No influxdb bucket was provided")?; // "web3_proxy";

    info!("Bucket is {:?}", bucket);
    let mut filter_chain_id = "".to_string();
    if chain_id != 0 {
        filter_chain_id = f!(r#"|> filter(fn: (r) => r["chain_id"] == "{chain_id}")"#);
    }

    // Fetch and request for balance

    info!(
        "Query start and stop are: {:?} {:?}",
        query_start, query_stop
    );
    // info!("Query column parameters are: {:?}", stats_column);
    info!("Query measurement is: {:?}", measurement);
    info!("Filters are: {:?}", filter_chain_id); // filter_field
    info!("window seconds are: {:?}", query_window_seconds);

    let drop_method = match stat_response_type {
        StatType::Aggregated => f!(r#"|> drop(columns: ["method"])"#),
        StatType::Detailed => "".to_string(),
    };

    let query = f!(r#"
    base = from(bucket: "{bucket}")
        |> range(start: {query_start}, stop: {query_stop})
        {rpc_key_filter}
        |> filter(fn: (r) => r["_measurement"] == "{measurement}")
        {filter_chain_id}
        {drop_method}

    base
        |> aggregateWindow(every: {query_window_seconds}s, fn: sum, createEmpty: false)
        |> pivot(rowKey: ["_time"], columnKey: ["_field"], valueColumn: "_value")
        |> drop(columns: ["balance"])
        |> group(columns: ["_time", "_measurement", "archive_needed", "chain_id", "error_response", "method", "rpc_secret_key_id"])
        |> sort(columns: ["frontend_requests"])
        |> map(fn:(r) => ({{ r with "sum_credits_used": float(v: r["sum_credits_used"]) }}))
        |> cumulativeSum(columns: ["backend_requests", "cache_hits", "cache_misses", "frontend_requests", "sum_credits_used", "sum_request_bytes", "sum_response_bytes", "sum_response_millis"])
        |> sort(columns: ["frontend_requests"], desc: true)
        |> limit(n: 1)
        |> group()
        |> sort(columns: ["_time", "_measurement", "archive_needed", "chain_id", "error_response", "method", "rpc_secret_key_id"], desc: true)
    "#);

    info!("Raw query to db is: {:?}", query);
    let query = Query::new(query.to_string());
    info!("Query to db is: {:?}", query);

    // Make the query and collect all data
    let raw_influx_responses: Vec<FluxRecord> = influxdb_client
        .query_raw(Some(query.clone()))
        .await
        .context("failed parsing query result into a FluxRecord")?;

    // Basically rename all items to be "total",
    // calculate number of "archive_needed" and "error_responses" through their boolean representations ...
    // HashMap<String, serde_json::Value>
    // let mut datapoints = HashMap::new();
    // TODO: I must be able to probably zip the balance query...
    let datapoints = raw_influx_responses
        .into_iter()
        // .into_values()
        .map(|x| x.values)
        .map(|value_map| {
            // Unwrap all relevant numbers
            // BTreeMap<String, value::Value>
            let mut out: HashMap<String, serde_json::Value> = HashMap::new();
            value_map.into_iter().for_each(|(key, value)| {
                if key == "_measurement" {
                    match value {
                        influxdb2_structmap::value::Value::String(inner) => {
                            if inner == "opt_in_proxy" {
                                out.insert(
                                    "collection".to_owned(),
                                    serde_json::Value::String("opt-in".to_owned()),
                                );
                            } else if inner == "global_proxy" {
                                out.insert(
                                    "collection".to_owned(),
                                    serde_json::Value::String("global".to_owned()),
                                );
                            } else {
                                warn!("Some datapoints are not part of any _measurement!");
                                out.insert(
                                    "collection".to_owned(),
                                    serde_json::Value::String("unknown".to_owned()),
                                );
                            }
                        }
                        _ => {
                            error!("_measurement should always be a String!");
                        }
                    }
                } else if key == "_stop" {
                    match value {
                        influxdb2_structmap::value::Value::TimeRFC(inner) => {
                            out.insert(
                                "stop_time".to_owned(),
                                serde_json::Value::String(inner.to_string()),
                            );
                        }
                        _ => {
                            error!("_stop should always be a TimeRFC!");
                        }
                    };
                } else if key == "_time" {
                    match value {
                        influxdb2_structmap::value::Value::TimeRFC(inner) => {
                            out.insert(
                                "time".to_owned(),
                                serde_json::Value::String(inner.to_string()),
                            );
                        }
                        _ => {
                            error!("_stop should always be a TimeRFC!");
                        }
                    }
                } else if key == "backend_requests" {
                    match value {
                        influxdb2_structmap::value::Value::Long(inner) => {
                            out.insert(
                                "total_backend_requests".to_owned(),
                                serde_json::Value::Number(inner.into()),
                            );
                        }
                        _ => {
                            error!("backend_requests should always be a Long!");
                        }
                    }
                } else if key == "balance" {
                    match value {
                        influxdb2_structmap::value::Value::Double(inner) => {
                            out.insert("balance".to_owned(), json!(f64::from(inner)));
                        }
                        _ => {
                            error!("balance should always be a Double!");
                        }
                    }
                } else if key == "cache_hits" {
                    match value {
                        influxdb2_structmap::value::Value::Long(inner) => {
                            out.insert(
                                "total_cache_hits".to_owned(),
                                serde_json::Value::Number(inner.into()),
                            );
                        }
                        _ => {
                            error!("cache_hits should always be a Long!");
                        }
                    }
                } else if key == "cache_misses" {
                    match value {
                        influxdb2_structmap::value::Value::Long(inner) => {
                            out.insert(
                                "total_cache_misses".to_owned(),
                                serde_json::Value::Number(inner.into()),
                            );
                        }
                        _ => {
                            error!("cache_misses should always be a Long!");
                        }
                    }
                } else if key == "frontend_requests" {
                    match value {
                        influxdb2_structmap::value::Value::Long(inner) => {
                            out.insert(
                                "total_frontend_requests".to_owned(),
                                serde_json::Value::Number(inner.into()),
                            );
                        }
                        _ => {
                            error!("frontend_requests should always be a Long!");
                        }
                    }
                } else if key == "no_servers" {
                    match value {
                        influxdb2_structmap::value::Value::Long(inner) => {
                            out.insert(
                                "no_servers".to_owned(),
                                serde_json::Value::Number(inner.into()),
                            );
                        }
                        _ => {
                            error!("no_servers should always be a Long!");
                        }
                    }
                } else if key == "sum_credits_used" {
                    match value {
                        influxdb2_structmap::value::Value::Double(inner) => {
                            out.insert("total_credits_used".to_owned(), json!(f64::from(inner)));
                        }
                        _ => {
                            error!("sum_credits_used should always be a Double!");
                        }
                    }
                } else if key == "sum_request_bytes" {
                    match value {
                        influxdb2_structmap::value::Value::Long(inner) => {
                            out.insert(
                                "total_request_bytes".to_owned(),
                                serde_json::Value::Number(inner.into()),
                            );
                        }
                        _ => {
                            error!("sum_request_bytes should always be a Long!");
                        }
                    }
                } else if key == "sum_response_bytes" {
                    match value {
                        influxdb2_structmap::value::Value::Long(inner) => {
                            out.insert(
                                "total_response_bytes".to_owned(),
                                serde_json::Value::Number(inner.into()),
                            );
                        }
                        _ => {
                            error!("sum_response_bytes should always be a Long!");
                        }
                    }
                } else if key == "rpc_secret_key_id" {
                    match value {
                        influxdb2_structmap::value::Value::String(inner) => {
                            out.insert(
                                "rpc_key".to_owned(),
                                serde_json::Value::String(
                                    rpc_key_id_to_key.get(&inner).unwrap().to_string(),
                                ),
                            );
                        }
                        _ => {
                            error!("rpc_secret_key_id should always be a String!");
                        }
                    }
                } else if key == "sum_response_millis" {
                    match value {
                        influxdb2_structmap::value::Value::Long(inner) => {
                            out.insert(
                                "total_response_millis".to_owned(),
                                serde_json::Value::Number(inner.into()),
                            );
                        }
                        _ => {
                            error!("sum_response_millis should always be a Long!");
                        }
                    }
                }
                // Make this if detailed ...
                else if stat_response_type == StatType::Detailed && key == "method" {
                    match value {
                        influxdb2_structmap::value::Value::String(inner) => {
                            out.insert("method".to_owned(), serde_json::Value::String(inner));
                        }
                        _ => {
                            error!("method should always be a String!");
                        }
                    }
                } else if key == "chain_id" {
                    match value {
                        influxdb2_structmap::value::Value::String(inner) => {
                            out.insert("chain_id".to_owned(), serde_json::Value::String(inner));
                        }
                        _ => {
                            error!("chain_id should always be a String!");
                        }
                    }
                } else if key == "archive_needed" {
                    match value {
                        influxdb2_structmap::value::Value::String(inner) => {
                            out.insert(
                                "archive_needed".to_owned(),
                                if inner == "true" {
                                    serde_json::Value::Bool(true)
                                } else if inner == "false" {
                                    serde_json::Value::Bool(false)
                                } else {
                                    serde_json::Value::String("error".to_owned())
                                },
                            );
                        }
                        _ => {
                            error!("archive_needed should always be a String!");
                        }
                    }
                } else if key == "error_response" {
                    match value {
                        influxdb2_structmap::value::Value::String(inner) => {
                            out.insert(
                                "error_response".to_owned(),
                                if inner == "true" {
                                    serde_json::Value::Bool(true)
                                } else if inner == "false" {
                                    serde_json::Value::Bool(false)
                                } else {
                                    serde_json::Value::String("error".to_owned())
                                },
                            );
                        }
                        _ => {
                            error!("error_response should always be a Long!");
                        }
                    }
                }
            });

            // datapoints.insert(out.get("time"), out);
            json!(out)
        })
        .collect::<Vec<_>>();

    // I suppose archive requests could be either gathered by default (then summed up), or retrieved on a second go.
    // Same with error responses ..
    let mut response_body = HashMap::new();
    response_body.insert(
        "num_items",
        serde_json::Value::Number(datapoints.len().into()),
    );
    response_body.insert("result", serde_json::Value::Array(datapoints));
    response_body.insert(
        "query_window_seconds",
        serde_json::Value::Number(query_window_seconds.into()),
    );
    response_body.insert("query_start", serde_json::Value::Number(query_start.into()));
    response_body.insert("chain_id", serde_json::Value::Number(chain_id.into()));

    if user_id == 0 {
        // 0 means everyone. don't filter on user
    } else {
        response_body.insert("user_id", serde_json::Value::Number(user_id.into()));
    }

    // Also optionally add the rpc_key_id:
    if let Some(rpc_key_id) = params.get("rpc_key_id") {
        let rpc_key_id = rpc_key_id
            .parse::<u64>()
            .map_err(|_| Web3ProxyError::BadRequest("Unable to parse rpc_key_id".to_string()))?;
        response_body.insert("rpc_key_id", serde_json::Value::Number(rpc_key_id.into()));
    }

    let response = Json(json!(response_body)).into_response();
    // Add the requests back into out

    // TODO: Now impplement the proper response type

    Ok(response)
}
