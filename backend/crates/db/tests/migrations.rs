//! 마이그레이션을 실제 PG에 적용해 본다 — `sqlx::migrate!`의 컴파일 타임
//! 파싱은 SQL 실행 오류(문법·제약 충돌)를 잡지 못한다. CI가 띄운 PG의
//! `DATABASE_URL`로 돌고, 없으면 조용히 스킵한다 (로컬 무의존).
//! 이미 적용된 DB에서는 체크섬 검증 no-op이라 재실행도 안전하다.

#![allow(clippy::expect_used)]

#[tokio::test]
async fn migrations_apply_cleanly() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = filegate_db::connect(&url, 2).await.expect("connect");
    filegate_db::migrate(&pool).await.expect("migrations apply");
}
