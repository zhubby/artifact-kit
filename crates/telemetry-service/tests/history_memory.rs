//! Measure the real historical-query path with a result far larger than its cap.
use artifactory_database::{PoolSettings, connect, params, sql, with_options};
use artifactory_telemetry_service::{
    query::{Range, Resolution},
    store::TelemetryStore,
};
use chrono::{Duration, Utc};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;

struct Tracked;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
#[global_allocator]
static ALLOCATOR: Tracked = Tracked;

fn allocated(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::SeqCst) + size;
    PEAK.fetch_max(live, Ordering::SeqCst);
}

// SAFETY: every allocation is delegated unchanged to System, with the caller's
// original layout. The atomics only count successfully allocated bytes.
unsafe impl GlobalAlloc for Tracked {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            allocated(layout.size());
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            allocated(layout.size());
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !ptr.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
            allocated(new_size);
        }
        ptr
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL; measures client allocations"]
async fn history_memory_is_bounded_with_large_complete_results() {
    let url = std::env::var("ARTIFACTORY_TEST_DATABASE_URL").unwrap();
    let admin = connect(&url, Default::default(), PoolSettings::new(1))
        .await
        .unwrap();
    let schema = format!("memory_{}", Uuid::new_v4().simple());
    sql::execute(
        &mut admin.clone(),
        &format!("CREATE SCHEMA {schema}"),
        params![],
    )
    .await
    .unwrap();
    let db = connect(
        &with_options(&url, &[("search_path", &schema)]).unwrap(),
        Default::default(),
        PoolSettings::new(1),
    )
    .await
    .unwrap();
    sql::execute(&mut db.clone(), "CREATE TABLE process_samples(sampled_at timestamptz,attempt_id uuid,pid bigint,status text)", params![]).await.unwrap();
    let attempt = Uuid::new_v4();
    let at = Utc::now();
    let range = Range {
        from: at - Duration::minutes(1),
        to: at + Duration::minutes(1),
        resolution: Resolution::Raw,
    };
    let store = TelemetryStore::new(db.clone(), db.clone());
    let mut previous_peak = 0;
    for (start, end) in [(1i32, 6001i32), (6002, 12001)] {
        sql::execute(&mut db.clone(), "INSERT INTO process_samples SELECT $1,$2,i,repeat('x',65536) FROM generate_series($3::integer,$4::integer) i", params![at,attempt,start,end]).await.unwrap();
        let base = LIVE.load(Ordering::SeqCst);
        PEAK.store(base, Ordering::SeqCst);
        let page = store
            .processes(attempt, &range)
            .await
            .unwrap_or_else(|error| match error {
                artifactory_runtime_protocol::error::ApiError::Internal(error) => {
                    panic!("{error:#}")
                }
                _ => panic!("history query failed"),
            });
        let peak = PEAK.load(Ordering::SeqCst).saturating_sub(base);
        assert!(page.truncated);
        assert!(page.samples.len() < 33);
        assert!(
            peak < 64 * 1024 * 1024,
            "buffered {peak} bytes for {end} rows"
        );
        if previous_peak != 0 {
            assert!(
                peak <= previous_peak + 4 * 1024 * 1024,
                "client memory grew with the complete result"
            );
        }
        println!(
            "HISTORY_MEMORY rows={end} complete_bytes={} peak_client_bytes={peak} returned_rows={}",
            end as usize * 65536,
            page.samples.len()
        );
        previous_peak = peak;
    }
    sql::execute(
        &mut admin.clone(),
        &format!("DROP SCHEMA {schema} CASCADE"),
        params![],
    )
    .await
    .unwrap();
}
