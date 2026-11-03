use super::*;

async fn water_of(pool: &PgPool, id: i64) -> f64 {
    sqlx::query_scalar("SELECT (water + water_fraction)::float8 FROM players WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn sleeping_characters_keep_their_water_and_food_and_drink_restore_it(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let token = create_session(&router, "sleeper").await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Hours without a client or movement cost nothing: the character is asleep.
    sqlx::query("UPDATE players SET water = 60, water_fraction = 0, last_active_at = now() - interval '5 hours', last_seen_at = now() - interval '5 hours', survival_updated_at = now() - interval '5 hours' WHERE id = $1")
        .bind(id).execute(&pool).await.unwrap();
    survival::settle_world(&state).await.unwrap();
    assert!((water_of(&pool, id).await - 60.0).abs() < 1e-6);
    let health: i32 = sqlx::query_scalar("SELECT health FROM players WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(health, 100, "no dehydration while asleep");

    // A character whose client is polling is awake and thirsty: 100 s cost one point.
    sqlx::query("UPDATE players SET last_seen_at = now(), survival_updated_at = now() - interval '100 seconds' WHERE id = $1")
        .bind(id).execute(&pool).await.unwrap();
    survival::settle_world(&state).await.unwrap();
    let awake = water_of(&pool, id).await;
    assert!((awake - 59.0).abs() < 0.05, "awake drain: {awake}");

    // Fruit holds water; bread and cheese hardly any; the reserve never exceeds 100.
    sqlx::query("UPDATE players SET water = 50, water_fraction = 0 WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let profile = response_json(
        player_request(
            &router,
            "POST",
            "/players/me/eat",
            r#"{"item_id":"apple"}"#,
            &token,
        )
        .await,
    )
    .await;
    assert!(
        (profile["stats"]["water"].as_f64().unwrap() - 58.0).abs() < 1.5,
        "{}",
        profile["stats"]
    );
    let apple = profile["inventory"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["item_id"] == "apple")
        .unwrap();
    assert_eq!(
        apple["water"], 8,
        "clients are told how much water food holds"
    );
    sqlx::query("UPDATE players SET water = 97, water_fraction = 0.5 WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let profile = response_json(
        player_request(
            &router,
            "POST",
            "/players/me/eat",
            r#"{"item_id":"apple"}"#,
            &token,
        )
        .await,
    )
    .await;
    assert!(
        profile["stats"]["water"].as_i64().unwrap() >= 99,
        "{}",
        profile["stats"]
    );
    assert!(water_of(&pool, id).await > 99.9 && water_of(&pool, id).await <= 100.0 + 1e-9);

    // Drinking needs fresh water within reach; the spawn of this test world has none.
    let refused = player_request(&router, "POST", "/players/me/drink", "", &token).await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    let unauthenticated = player_request(&router, "POST", "/players/me/drink", "", "").await;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn a_character_in_water_loses_health_by_fractions_and_drowns(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    create_session(&router, "swimmer").await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();

    // One step of a quarter second costs less than a point, and the rest is not lost.
    let mut transaction = pool.begin().await.unwrap();
    assert!(!survival::drown(&state, &mut transaction, id, 0.25)
        .await
        .unwrap());
    transaction.commit().await.unwrap();
    let health: f64 =
        sqlx::query_scalar("SELECT (health + health_fraction)::float8 FROM players WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!((health - (100.0 - 0.25 * 100.0 / 60.0)).abs() < 1e-6);

    // About a minute in the water kills for good, with its own cause.
    let mut died = false;
    for _ in 0..300 {
        let mut transaction = pool.begin().await.unwrap();
        died = survival::drown(&state, &mut transaction, id, 0.25)
            .await
            .unwrap();
        transaction.commit().await.unwrap();
        if died {
            break;
        }
    }
    assert!(died);
    let cause: String = sqlx::query_scalar("SELECT cause FROM graves WHERE player_id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(cause, "drowning");
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn a_waterskin_holds_five_litres_and_is_drunk_from_anywhere(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let token = create_session(&router, "nomad").await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    let skin = |profile: &serde_json::Value| {
        profile["inventory"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["item_id"] == "waterskin")
            .cloned()
    };

    // Without a skin there is nothing to fill or drink from.
    let response = player_request(&router, "POST", "/players/me/waterskin/drink", "", &token).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);

    super::building::give(&pool, "nomad", &[("waterskin", 1)]).await;
    sqlx::query("UPDATE players SET water = 20, water_fraction = 0 WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

    // A new skin is empty and says what it can hold.
    let response = player_request(&router, "POST", "/players/me/waterskin/drink", "", &token).await;
    assert_eq!(
        response.status(),
        StatusCode::CONFLICT,
        "an empty skin gives nothing"
    );
    let profile =
        response_json(player_request(&router, "GET", "/players/me", "", &token).await).await;
    let item = skin(&profile).expect("the waterskin is in the inventory");
    assert_eq!(item["capacity_ml"], 5000);
    assert_eq!(item["fill_ml"], 0);

    // Full (as filling at the water leaves it), one drink is half a litre: ten points of the reserve.
    sqlx::query("INSERT INTO player_waterskin (player_id, water_ml) VALUES ($1, 5000)")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let profile = response_json(
        player_request(&router, "POST", "/players/me/waterskin/drink", "", &token).await,
    )
    .await;
    assert!(
        (profile["stats"]["water"].as_f64().unwrap() - 30.0).abs() < 1.5,
        "{}",
        profile["stats"]
    );
    assert_eq!(skin(&profile).unwrap()["fill_ml"], 4500);

    // Nobody drinks more than the reserve holds, and a full character is not thirsty.
    sqlx::query("UPDATE players SET water = 99, water_fraction = 0 WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let profile = response_json(
        player_request(&router, "POST", "/players/me/waterskin/drink", "", &token).await,
    )
    .await;
    assert!(profile["stats"]["water"].as_f64().unwrap() <= 100.0);
    assert_eq!(skin(&profile).unwrap()["fill_ml"], 4450);
    sqlx::query("UPDATE players SET water = 100, water_fraction = 0 WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let response = player_request(&router, "POST", "/players/me/waterskin/drink", "", &token).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);

    // Filling needs fresh water within reach; the spawn is on dry ground.
    let response = player_request(&router, "POST", "/players/me/waterskin/fill", "", &token).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);

    // A skin that leaves the inventory takes its water with it.
    sqlx::query("DELETE FROM player_inventory WHERE player_id = $1 AND item_id = 'waterskin'")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let left: Option<i32> =
        sqlx::query_scalar("SELECT water_ml FROM player_waterskin WHERE player_id = $1")
            .bind(id)
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert!(left.is_none());
}
