//! fgclient — filegate 사용자 도구 + e2e 하버스.
//!
//! 사용:
//!   fgclient put <파일> --intent <intent> [--type <content-type>]
//!   fgclient get <file_id> -o <파일> [--name <다운로드명>]
//!   fgclient stat <file_id>
//!   fgclient rm <file_id>
//!   fgclient e2e            # 케이스 트리 전체를 세 모드로 실측 (로컬 개발 DB 전용)
//!
//! env: FILEGATE_URL(기본 127.0.0.1:8080), FILEGATE_CLIENT_KEY(기본 로컬 개발 키).

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
//! 하버스는 로컬 개발 전용 도구다 — 실패는 즉시 드러나야 하므로 unwrap을 쓴다.

use filegate_client::{md5_hex, FilegateClient};

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let base = env("FILEGATE_URL", "http://127.0.0.1:8080");
    let key = env(
        "FILEGATE_CLIENT_KEY",
        "fg_local-dev-notegate-key-0123456789abcdef",
    );
    let client = FilegateClient::new(&base, &key);

    let code = match args.get(1).map(String::as_str) {
        Some("put") => cmd_put(&client, &args).await,
        Some("get") => cmd_get(&client, &args).await,
        Some("stat") => cmd_stat(&client, &args).await,
        Some("rm") => cmd_rm(&client, &args).await,
        Some("e2e") => e2e(&base, &key).await,
        _ => {
            eprintln!("usage: fgclient put|get|stat|rm|e2e ...");
            2
        }
    };
    std::process::exit(code);
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

async fn cmd_put(client: &FilegateClient, args: &[String]) -> i32 {
    let Some(path) = args.get(2) else {
        eprintln!("put <파일> --intent <intent>");
        return 2;
    };
    let intent = flag(args, "--intent").unwrap_or("attachment");
    let ct = flag(args, "--type");
    match client.upload_file(path, intent, ct).await {
        Ok(id) => {
            println!("{id}");
            0
        }
        Err(e) => {
            eprintln!("put 실패: {e}");
            1
        }
    }
}

async fn cmd_get(client: &FilegateClient, args: &[String]) -> i32 {
    let Some(id) = args.get(2) else {
        eprintln!("get <file_id> -o <파일>");
        return 2;
    };
    let out = flag(args, "-o").unwrap_or("out.bin");
    let name = flag(args, "--name");
    match client.download_file(id, out, name).await {
        Ok(()) => {
            println!("→ {out}");
            0
        }
        Err(e) => {
            eprintln!("get 실패: {e}");
            1
        }
    }
}

async fn cmd_stat(client: &FilegateClient, args: &[String]) -> i32 {
    let Some(id) = args.get(2) else {
        return 2;
    };
    match client.stat(id).await {
        Ok(out) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&out.body).unwrap_or_default()
            );
            i32::from(out.status != 200)
        }
        Err(e) => {
            eprintln!("stat 실패: {e}");
            1
        }
    }
}

async fn cmd_rm(client: &FilegateClient, args: &[String]) -> i32 {
    let Some(id) = args.get(2) else {
        return 2;
    };
    match client.delete(id).await {
        Ok(code) => {
            println!("delete {code}");
            i32::from(code != 200)
        }
        Err(e) => {
            eprintln!("rm 실패: {e}");
            1
        }
    }
}

// ================= e2e 하버스 =================

struct Harness {
    pass: u32,
    fail: u32,
}

impl Harness {
    fn ok(&mut self, _name: &str) {
        self.pass += 1;
    }
    fn bad(&mut self, name: &str, detail: String) {
        self.fail += 1;
        println!("FAIL: {name} — {detail}");
    }
    fn eq<T: PartialEq + std::fmt::Debug>(&mut self, name: &str, want: T, got: T) {
        if want == got {
            self.ok(name);
        } else {
            self.bad(name, format!("want {want:?}, got {got:?}"));
        }
    }
    fn is_true(&mut self, name: &str, cond: bool, detail: impl FnOnce() -> String) {
        if cond {
            self.ok(name);
        } else {
            self.bad(name, detail());
        }
    }
}

fn rand_bytes(n: usize) -> Vec<u8> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf
}

/// 로컬 개발 DB에 psql — 백그라운드 검증·강제 만료용 (셸 스위트와 같은 방식).
fn psql(sql: &str) -> String {
    let container = env("FILEGATE_PG_CONTAINER", "filegate-postgres-1");
    let out = std::process::Command::new("docker")
        .args([
            "exec", &container, "psql", "-U", "filegate", "-d", "filegate", "-qtc", sql,
        ])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        Err(_) => String::new(),
    }
}

/// (intent, storage_id)
struct Mode {
    intent: &'static str,
    storage: &'static str,
}

async fn e2e(base: &str, key: &str) -> i32 {
    let c = FilegateClient::new(base, key);
    let mut h = Harness { pass: 0, fail: 0 };
    let modes = [
        Mode {
            intent: "attachment",
            storage: "minio-local",
        },
        Mode {
            intent: "relay-att",
            storage: "minio-relay",
        },
        Mode {
            intent: "fs-att",
            storage: "fs-local",
        },
    ];

    section("0. 인증");
    auth_cases(base, &mut h).await;

    section("A. 단일 파일 — 정상 (3 모드)");
    for m in &modes {
        single_happy(&c, m, &mut h).await;
    }
    section("B. 단일 파일 — 실패·엣지");
    single_create_rejects(&c, &mut h).await;
    // PUT 거부는 중계에서만 (직결은 벤더가 처리) — relay-att로 대표 검증
    single_relay_put_rejects(&c, &mut h).await;
    single_commit_rejects(&c, &mut h).await;

    section("C. multipart — 정상 (3 모드)");
    for m in &modes {
        multipart_happy(&c, m, &mut h).await;
    }
    section("D. multipart — 실패·엣지·재개");
    multipart_rejects(&c, &mut h).await;

    section("F. 다운로드 — 실패");
    download_rejects(&c, &mut h).await;

    section("G. 삭제·purge");
    delete_purge(&c, &mut h).await;

    section("H. 회수·찌꺼기 (강제 만료)");
    reclaim_cases(&c, &mut h).await;

    println!("\n결과: PASS={} FAIL={}", h.pass, h.fail);
    i32::from(h.fail > 0)
}

fn section(title: &str) {
    println!("\n=== {title} ===");
}

async fn auth_cases(base: &str, h: &mut Harness) {
    let no_key = FilegateClient::with_key(base, None);
    let created = no_key.create("attachment", 1, None, None).await.unwrap();
    h.eq("키 없음 → 401", 401, created.status);
    let wrong = FilegateClient::with_key(base, Some("fg_wrong-key".into()));
    let created = wrong.create("attachment", 1, None, None).await.unwrap();
    h.eq("틀린 키 → 401", 401, created.status);
}

async fn single_happy(c: &FilegateClient, m: &Mode, h: &mut Harness) {
    let payload = format!("hello filegate — {}", m.storage).into_bytes();
    let md5 = md5_hex(&payload);
    let created = c
        .create(
            m.intent,
            payload.len() as i64,
            Some("text/plain"),
            Some(&md5),
        )
        .await
        .unwrap();
    h.eq(&f(m, "create 201"), 201, created.status);
    let file_id = created.str("file_id").unwrap_or("").to_owned();
    let url = created.str("put_url").unwrap_or("").to_owned();
    h.is_true(&f(m, "put_url 있음"), !url.is_empty(), || {
        format!("{:?}", created.body)
    });
    // 키 규약 경로는 직결 presigned URL에만 드러난다 (중계는 /b 엔드포인트라 키가 URL에 없음).
    if m.storage == "minio-local" {
        h.is_true(
            &f(m, "직결 키 규약 경로"),
            url.contains("/fg/notegate/"),
            || url.clone(),
        );
    }

    let (code, etag) = c
        .put(&url, payload.clone(), Some("text/plain"))
        .await
        .unwrap();
    h.eq(&f(m, "PUT 200"), 200, code);
    let committed = c.commit(&file_id).await.unwrap();
    h.eq(&f(m, "commit 200"), 200, committed.status);
    h.eq(
        &f(m, "commit ETag=MD5"),
        Some(md5.as_str()),
        committed.str("etag"),
    );
    let _ = etag;

    // 다운로드 왕복 (F 합침)
    let read = c.read(&file_id, Some("모드 v1+2&3#.txt")).await.unwrap();
    h.eq(&f(m, "read 200"), 200, read.status);
    let get_url = read.str("get_url").unwrap_or("").to_owned();
    let got = c.get(&get_url).await.unwrap();
    h.eq(&f(m, "GET md5 왕복"), md5.clone(), md5_hex(&got.bytes));
    let dispo = got.disposition.unwrap_or_default();
    h.is_true(
        &f(m, "RFC5987 파일명 왕복"),
        dispo.contains("v1+2&3#"),
        || dispo.clone(),
    );

    let st = c.stat(&file_id).await.unwrap();
    h.eq(&f(m, "stat active"), Some("active"), st.str("state"));

    // 회계 active
    let active = psql(&format!(
        "SELECT active_bytes FROM storage_usage WHERE storage_id='{}';",
        m.storage
    ));
    h.eq(&f(m, "회계 active"), payload.len().to_string(), active);

    // 정리
    let _ = c.delete(&file_id).await;
}

async fn single_create_rejects(c: &FilegateClient, h: &mut Harness) {
    h.eq(
        "없는 intent → 404",
        404,
        c.create("nope-intent", 1, None, None).await.unwrap().status,
    );
    h.eq(
        "음수 size → 400",
        400,
        c.create("attachment", -1, None, None).await.unwrap().status,
    );
    // part_size(5MiB) × 10000 ≈ 50GiB 초과는 multipart 한계 → 400 (capacity 예약 전에 거부).
    h.eq(
        "multipart 한계 초과 → 400",
        400,
        c.create("attachment", 60 * 1024 * 1024 * 1024, None, None)
            .await
            .unwrap()
            .status,
    );
    h.eq(
        "제어문자 content_type → 400",
        400,
        c.create("attachment", 1, Some("a\x01b"), None)
            .await
            .unwrap()
            .status,
    );
    // capacity 초과는 storage capacity에 의존 — minio-local capacity를 넘는 값
    let cap = psql("SELECT capacity_bytes FROM storages WHERE id='minio-local';")
        .parse::<i64>()
        .unwrap_or(0);
    if cap > 0 {
        h.eq(
            "capacity 초과 → 507",
            507,
            c.create("attachment", cap + 1, None, None)
                .await
                .unwrap()
                .status,
        );
    }
}

async fn single_relay_put_rejects(c: &FilegateClient, h: &mut Harness) {
    // 새 relay 단일 파일 하나로 PUT 거부들을 찌른다.
    let created = c.create("relay-att", 10, None, None).await.unwrap();
    let file_id = created.str("file_id").unwrap_or("").to_owned();
    let url = created.str("put_url").unwrap_or("").to_owned();
    let lease = url
        .split("/b/")
        .nth(1)
        .and_then(|s| s.split('?').next())
        .unwrap_or("")
        .to_owned();

    h.eq(
        "CL ≠ 선언 → 400",
        400,
        c.put(&url, vec![1u8; 5], None).await.unwrap().0,
    );
    // 정상 클라이언트의 초과 업로드는 CL이 붙어 400(불일치)이다. 413(스트림 컷)은
    // CL을 속이는 악성 클라이언트 전용 방어라 여기선 관측되지 않는다.
    h.eq(
        "초과 업로드(정상 클라) → 400",
        400,
        c.put(&url, vec![1u8; 20], None).await.unwrap().0,
    );
    h.eq(
        "chunked(CL 없음) → 411",
        411,
        c.put_chunked(&url, vec![1u8; 10]).await.unwrap(),
    );
    let wrong = format!("{}/b/{lease}?s=wrongsecret", base_of(&url));
    h.eq(
        "틀린 secret → 403",
        403,
        c.put(&wrong, vec![1u8; 10], None).await.unwrap().0,
    );
    let nolease = format!(
        "{}/b/00000000-0000-0000-0000-000000000000?s=x",
        base_of(&url)
    );
    h.eq(
        "없는 lease → 403",
        403,
        c.put(&nolease, vec![1u8; 10], None).await.unwrap().0,
    );
    // 만료 lease
    psql(&format!(
        "UPDATE leases SET expires_at = now() - interval '1 second' WHERE id='{lease}';"
    ));
    h.eq(
        "만료 lease → 403",
        403,
        c.put(&url, vec![1u8; 10], None).await.unwrap().0,
    );
    let _ = c.delete(&file_id).await;
}

async fn single_commit_rejects(c: &FilegateClient, h: &mut Harness) {
    // 업로드 전 commit
    let created = c.create("relay-att", 10, None, None).await.unwrap();
    let file_id = created.str("file_id").unwrap_or("").to_owned();
    h.eq(
        "업로드 전 commit → 400",
        400,
        c.commit(&file_id).await.unwrap().status,
    );
    // md5 불일치
    let url = created.str("put_url").unwrap_or("").to_owned();
    let created2 = c
        .create(
            "relay-att",
            9,
            None,
            Some("00000000000000000000000000000000"),
        )
        .await
        .unwrap();
    let fid2 = created2.str("file_id").unwrap_or("").to_owned();
    let u2 = created2.str("put_url").unwrap_or("").to_owned();
    c.put(&u2, b"123456789".to_vec(), None).await.unwrap();
    h.eq(
        "md5 불일치 commit → 400",
        400,
        c.commit(&fid2).await.unwrap().status,
    );
    h.eq(
        "불일치 후 pending 유지",
        "pending".to_string(),
        psql(&format!("SELECT state FROM files WHERE id='{fid2}';")),
    );
    // 없는 파일 commit
    h.eq(
        "없는 file commit → 404",
        404,
        c.commit("00000000-0000-0000-0000-000000000000")
            .await
            .unwrap()
            .status,
    );
    // 중복 commit (멱등) — 정상 파일로
    let payload = b"idem".to_vec();
    let cr = c
        .create("relay-att", 4, None, Some(&md5_hex(&payload)))
        .await
        .unwrap();
    let fid = cr.str("file_id").unwrap_or("").to_owned();
    c.put(cr.str("put_url").unwrap_or(""), payload, None)
        .await
        .unwrap();
    h.eq("commit 200", 200, c.commit(&fid).await.unwrap().status);
    h.eq(
        "중복 commit 멱등 200",
        200,
        c.commit(&fid).await.unwrap().status,
    );
    // 삭제된 파일 commit
    c.delete(&fid).await.unwrap();
    h.eq(
        "삭제된 파일 commit → 409",
        409,
        c.commit(&fid).await.unwrap().status,
    );
    // 정리: pending 둘(업로드 전 file_id, md5 불일치 fid2)의 예약을 만료로 회수시킨다.
    let _ = url;
    psql(&format!("UPDATE leases SET expires_at = now() - interval '1 second' WHERE file_id IN ('{file_id}','{fid2}');"));
}

async fn multipart_happy(c: &FilegateClient, m: &Mode, h: &mut Harness) {
    let size = 12 * 1024 * 1024_i64;
    let bytes = rand_bytes(size as usize);
    let whole = md5_hex(&bytes);
    let created = c
        .create(m.intent, size, Some("application/zip"), None)
        .await
        .unwrap();
    h.eq(&f(m, "mp create 201"), 201, created.status);
    let file_id = created.str("file_id").unwrap_or("").to_owned();
    h.is_true(
        &f(m, "put_url 없음(서술자)"),
        created.str("put_url").is_none(),
        || format!("{:?}", created.body),
    );
    let (part_size, part_count) = created.multipart().unwrap_or((0, 0));
    h.is_true(
        &f(m, "서술자 5MiB×3"),
        part_size == 5 * 1024 * 1024 && part_count == 3,
        || format!("{part_size}/{part_count}"),
    );

    let issued = c.parts(&file_id, &[1, 2, 3]).await.unwrap();
    // 순서 무관: 3 → 1 → 2
    for n in [3, 1, 2] {
        let start = ((n - 1) as i64 * part_size) as usize;
        let end = (start + part_size as usize).min(bytes.len());
        let url = FilegateClient::part_url(&issued, n).unwrap_or_default();
        let (code, _) = c.put(&url, bytes[start..end].to_vec(), None).await.unwrap();
        h.eq(&f(m, &format!("part{n} PUT 200")), 200, code);
    }
    let committed = c.commit(&file_id).await.unwrap();
    h.eq(&f(m, "mp commit 200"), 200, committed.status);
    h.is_true(
        &f(m, "합성 ETag -N"),
        committed.str("etag").unwrap_or("").ends_with("-3"),
        || format!("{:?}", committed.str("etag")),
    );

    // 다운로드 왕복
    let read = c.read(&file_id, None).await.unwrap();
    let got = c.get(read.str("get_url").unwrap_or("")).await.unwrap();
    h.eq(&f(m, "mp GET md5 왕복"), whole, md5_hex(&got.bytes));
    let _ = c.delete(&file_id).await;
}

async fn multipart_rejects(c: &FilegateClient, h: &mut Harness) {
    let size = 12 * 1024 * 1024_i64;
    // create 거부
    h.eq(
        "mp declared_md5 → 400",
        400,
        c.create(
            "relay-att",
            size,
            None,
            Some("00000000000000000000000000000000"),
        )
        .await
        .unwrap()
        .status,
    );
    // 정상 mp 하나로 parts/part 거부와 재개
    let bytes = rand_bytes(size as usize);
    let part = 5 * 1024 * 1024_i64;
    let created = c.create("relay-att", size, None, None).await.unwrap();
    let file_id = created.str("file_id").unwrap_or("").to_owned();
    h.eq(
        "part 범위 초과 발급 → 400",
        400,
        c.parts(&file_id, &[4]).await.unwrap().status,
    );

    let issued = c.parts(&file_id, &[1, 2, 3]).await.unwrap();
    let u1 = FilegateClient::part_url(&issued, 1).unwrap_or_default();
    let u2 = FilegateClient::part_url(&issued, 2).unwrap_or_default();
    // part= 없는 PUT → 400
    let u1_nopart = u1.replace("&part=1", "");
    h.eq(
        "part= 없는 PUT → 400",
        400,
        c.put(&u1_nopart, bytes[0..part as usize].to_vec(), None)
            .await
            .unwrap()
            .0,
    );
    // CL ≠ part 크기 → 400 (part1에 2MiB만)
    h.eq(
        "part 크기 불일치 → 400",
        400,
        c.put(&u2, bytes[0..(2 * 1024 * 1024)].to_vec(), None)
            .await
            .unwrap()
            .0,
    );
    // part1, part3 올리고 part2 없이 commit → 400
    c.put(&u1, bytes[0..part as usize].to_vec(), None)
        .await
        .unwrap();
    let u3 = FilegateClient::part_url(&issued, 3).unwrap_or_default();
    c.put(&u3, bytes[(2 * part as usize)..bytes.len()].to_vec(), None)
        .await
        .unwrap();
    h.eq(
        "part 누락 commit → 400",
        400,
        c.commit(&file_id).await.unwrap().status,
    );
    // 재개: part2 재발급 후 앞 배치 u1 여전히 유효
    let reissue = c.parts(&file_id, &[2]).await.unwrap();
    let u2b = FilegateClient::part_url(&reissue, 2).unwrap_or_default();
    h.eq(
        "재발급 part2 PUT 200",
        200,
        c.put(
            &u2b,
            bytes[(part as usize)..(2 * part as usize)].to_vec(),
            None,
        )
        .await
        .unwrap()
        .0,
    );
    h.eq(
        "재발급 후 앞 배치 URL 생존(비회전)",
        200,
        c.put(&u1, bytes[0..part as usize].to_vec(), None)
            .await
            .unwrap()
            .0,
    );
    // 이제 완성 → commit 200
    h.eq(
        "완성 commit 200",
        200,
        c.commit(&file_id).await.unwrap().status,
    );
    let _ = c.delete(&file_id).await;
}

async fn download_rejects(c: &FilegateClient, h: &mut Harness) {
    // pending 파일 read → 409
    let created = c.create("relay-att", 10, None, None).await.unwrap();
    let file_id = created.str("file_id").unwrap_or("").to_owned();
    h.eq(
        "pending read → 409",
        409,
        c.read(&file_id, None).await.unwrap().status,
    );
    h.eq(
        "없는 파일 read → 404",
        404,
        c.read("00000000-0000-0000-0000-000000000000", None)
            .await
            .unwrap()
            .status,
    );
    // 삭제된 파일 read → 409 (active였다가 delete)
    let payload = b"todelete".to_vec();
    let cr = c
        .create("relay-att", 8, None, Some(&md5_hex(&payload)))
        .await
        .unwrap();
    let fid = cr.str("file_id").unwrap_or("").to_owned();
    c.put(cr.str("put_url").unwrap_or(""), payload, None)
        .await
        .unwrap();
    c.commit(&fid).await.unwrap();
    c.delete(&fid).await.unwrap();
    h.eq(
        "삭제된 파일 read → 409",
        409,
        c.read(&fid, None).await.unwrap().status,
    );
    // 정리
    psql(&format!(
        "UPDATE leases SET expires_at = now() - interval '1 second' WHERE file_id='{file_id}';"
    ));
}

async fn delete_purge(c: &FilegateClient, h: &mut Harness) {
    // delete 멱등, pending 삭제 409, 없는 파일 404
    let created = c.create("relay-att", 10, None, None).await.unwrap();
    let file_id = created.str("file_id").unwrap_or("").to_owned();
    h.eq("pending 삭제 → 409", 409, c.delete(&file_id).await.unwrap());
    h.eq(
        "없는 파일 삭제 → 404",
        404,
        c.delete("00000000-0000-0000-0000-000000000000")
            .await
            .unwrap(),
    );
    // active → delete → purge → stat=deleted
    let payload = b"purge me".to_vec();
    let cr = c
        .create("relay-att", 8, None, Some(&md5_hex(&payload)))
        .await
        .unwrap();
    let fid = cr.str("file_id").unwrap_or("").to_owned();
    c.put(cr.str("put_url").unwrap_or(""), payload, None)
        .await
        .unwrap();
    c.commit(&fid).await.unwrap();
    h.eq("delete 200", 200, c.delete(&fid).await.unwrap());
    h.eq("delete 멱등 200", 200, c.delete(&fid).await.unwrap());
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    let st = c.stat(&fid).await.unwrap();
    h.eq("purge 후 stat=deleted", Some("deleted"), st.str("state"));
    // 정리
    psql(&format!(
        "UPDATE leases SET expires_at = now() - interval '1 second' WHERE file_id='{file_id}';"
    ));
}

async fn reclaim_cases(c: &FilegateClient, h: &mut Harness) {
    // pending 만료 → reclaimed + reserved 해제
    let created = c.create("relay-att", 100, None, None).await.unwrap();
    let file_id = created.str("file_id").unwrap_or("").to_owned();
    let reserved_before =
        psql("SELECT reserved_bytes FROM storage_usage WHERE storage_id='minio-relay';");
    h.is_true(
        "만료 전 reserved>0",
        reserved_before.parse::<i64>().unwrap_or(0) >= 100,
        || reserved_before.clone(),
    );
    psql(&format!(
        "UPDATE leases SET expires_at = now() - interval '1 second' WHERE file_id='{file_id}';"
    ));
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    h.eq(
        "pending → reclaimed",
        "reclaimed".to_string(),
        psql(&format!("SELECT state FROM files WHERE id='{file_id}';")),
    );
    // 종료 lease GC 강제
    let terminal_before = psql("SELECT count(*) FROM leases WHERE state <> 'issued';");
    if terminal_before.parse::<i64>().unwrap_or(0) > 0 {
        psql("UPDATE leases SET created_at = now() - interval '2 days' WHERE state <> 'issued';");
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        h.eq(
            "종료 lease GC → 0",
            "0".to_string(),
            psql("SELECT count(*) FROM leases WHERE state <> 'issued';"),
        );
        h.eq(
            "lease_parts CASCADE → 0",
            "0".to_string(),
            psql("SELECT count(*) FROM lease_parts;"),
        );
    }
    // 최종 회계 0 확인 (전 storage)
    let total = psql("SELECT coalesce(sum(reserved_bytes+active_bytes+purge_pending_bytes),0) FROM storage_usage;");
    h.eq("최종 회계 0", "0".to_string(), total);
}

fn f(m: &Mode, name: &str) -> String {
    format!("[{}] {name}", m.storage)
}

fn base_of(url: &str) -> String {
    // http://host:port/b/... → http://host:port
    url.split("/b/").next().unwrap_or(url).to_owned()
}
