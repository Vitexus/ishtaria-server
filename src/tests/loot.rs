use super::*;

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn dropped_items_lie_until_picked_or_scooped(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let token = create_session(&router, "dropper").await;
    let player_id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();

    // The starter axe is in hand: it cannot be thrown away until released.
    assert_eq!(
        player_request(
            &router,
            "POST",
            "/players/me/drop",
            r#"{"item_id":"axe","quantity":1}"#,
            &token
        )
        .await
        .status(),
        StatusCode::CONFLICT,
        "equipped"
    );
    assert_eq!(
        player_request(&router, "DELETE", "/players/me/equip", "", &token)
            .await
            .status(),
        StatusCode::OK
    );
    let dropped = response_json(
        player_request(
            &router,
            "POST",
            "/players/me/drop",
            r#"{"item_id":"axe","quantity":1}"#,
            &token,
        )
        .await,
    )
    .await;
    assert!(
        dropped["inventory"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["item_id"] != "axe"),
        "axe left the pack"
    );
    let piles: i64 = sqlx::query_scalar("SELECT count(*) FROM ground_items WHERE world_id = $1")
        .bind(world_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(piles, 1);

    let position: (f64, f64, f64) =
        sqlx::query_as("SELECT position_x, position_y, position_z FROM players WHERE id = $1")
            .bind(player_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let region = response_json(
        player_request(
            &router,
            "GET",
            &format!(
                "/world/objects?x={}&y={}&z={}",
                position.0, position.1, position.2
            ),
            "",
            &token,
        )
        .await,
    )
    .await;
    let loot = region["loot"].as_array().expect("loot list");
    assert_eq!(loot.len(), 1);
    assert_eq!(loot[0]["item_id"], "axe");
    let id = loot[0]["id"].as_i64().unwrap();

    let picked = response_json(
        player_request(&router, "POST", &format!("/ground/{id}/pickup"), "", &token).await,
    )
    .await;
    assert!(
        picked["inventory"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["item_id"] == "axe"),
        "axe returned"
    );
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM ground_items WHERE world_id = $1")
        .bind(world_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn only_a_viewer_who_is_there_moves_the_ground(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let _token = create_session(&router, "onlooker").await;
    let (id, position): (i64, (f64, f64, f64)) = {
        let row: (i64, f64, f64, f64) = sqlx::query_as(
            "SELECT id, position_x, position_y, position_z FROM players WHERE username = 'onlooker'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        (row.0, (row.1, row.2, row.3))
    };
    let here = [position.0, position.1, position.2];

    // Scooping and delivering follow a character who is standing there, because `/world/objects`
    // is a GET that anyone may call with any coordinates.
    assert!(
        crate::loot::viewer_is_there(&state, id, here)
            .await
            .unwrap(),
        "a character at the point is there"
    );

    // Five kilometres away is not "there", however the request is addressed.
    let far = [position.0 + 5000.0, position.1, position.2];
    assert!(
        !crate::loot::viewer_is_there(&state, id, far).await.unwrap(),
        "a character far from the point is not there"
    );

    // Neither is an unknown id (an anonymous request has none at all).
    assert!(
        !crate::loot::viewer_is_there(&state, id + 10_000, here)
            .await
            .unwrap(),
        "an unknown viewer is never there"
    );

    // A dead character does not walk NPCs over anybody's belongings either.
    // `player_death_complete` keeps `died_at` and `death_cause` together.
    sqlx::query("UPDATE players SET died_at = now(), death_cause = 'creature' WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        !crate::loot::viewer_is_there(&state, id, here)
            .await
            .unwrap(),
        "a dead character is not there"
    );
}
