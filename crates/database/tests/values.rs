use artifactory_database::{PoolSettings, connect, params, sql};
use chrono::{DateTime, Utc};
use serde_json::json;
use uuid::Uuid;

#[test]
fn timestamps_preserve_utc_microseconds_including_before_epoch() {
    for input in [
        "1969-12-31T23:59:59.999999Z",
        "2026-11-13T12:34:56.123456789Z",
    ] {
        let value: DateTime<Utc> = input.parse().unwrap();
        let converted = sql::chrono(sql::timestamp(value).unwrap()).unwrap();
        assert_eq!(converted.timestamp_micros(), value.timestamp_micros());
        assert_eq!(converted.timestamp_subsec_nanos() % 1000, 0);
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn typed_null_empty_uuid_arrays_and_json_roundtrip() {
    let mut db = connect(
        &std::env::var("ARTIFACTORY_TEST_DATABASE_URL").unwrap(),
        Default::default(),
        PoolSettings::new(1),
    )
    .await
    .unwrap();
    for ids in [
        None,
        Some(Vec::<Uuid>::new()),
        Some(vec![Uuid::new_v4(), Uuid::new_v4()]),
    ] {
        let result: Option<Vec<Uuid>> = sql::scalar(&mut db, "SELECT $1::uuid[]", params![&ids])
            .await
            .unwrap();
        assert_eq!(result, ids);
    }
    for value in [json!(null), json!({"nested":[null,1,true,"文本"]})] {
        let result: serde_json::Value = sql::scalar(&mut db, "SELECT $1::jsonb", params![&value])
            .await
            .unwrap();
        assert_eq!(result, value);
    }
    let absent: Option<serde_json::Value> = sql::scalar(
        &mut db,
        "SELECT $1::jsonb",
        params![None::<serde_json::Value>],
    )
    .await
    .unwrap();
    assert!(absent.is_none());
    let value: DateTime<Utc> = "1969-12-31T23:59:59.999999Z".parse().unwrap();
    let result: DateTime<Utc> = sql::scalar(&mut db, "SELECT $1::timestamptz", params![value])
        .await
        .unwrap();
    assert_eq!(result, value);
}
