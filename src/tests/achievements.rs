use super::harvest::{cooldown_reset, find_object, harvest_request, position_of, teleport};
use super::*;

async fn achievements_of(router: &Router, token: &str) -> serde_json::Value {
    response_json(player_request(router, "GET", "/players/me/achievements", "", token).await).await
}

fn earned_ids(reply: &serde_json::Value) -> Vec<String> {
    reply["earned"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap().to_string())
        .collect()
}

async fn achievement_events(router: &Router, token: &str) -> Vec<String> {
    let reply =
        response_json(player_request(router, "GET", "/events?after=0", "", token).await).await;
    reply["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "achievement")
        .map(|event| event["subject"].as_str().unwrap().to_string())
        .collect()
}

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn achievements_are_earned_once_and_announced_once(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let token = create_session(&router, "achiever").await;
    let player_id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();

    // A new character has nothing; hidden achievements show neither title nor description.
    let fresh = achievements_of(&router, &token).await;
    assert!(earned_ids(&fresh).is_empty());
    let catalog = fresh["achievements"].as_array().unwrap();
    let legend = catalog
        .iter()
        .find(|entry| entry["id"] == "level_30")
        .unwrap();
    assert_eq!(legend["hidden"], true);
    assert_eq!(legend["title"], "");
    assert_eq!(legend["description"], "");
    let public = response_json(
        router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/achievements")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        public["achievements"].as_array().unwrap().len(),
        catalog.len()
    );

    // The first swing earns the woodcutter; the second does not earn it again.
    let spawn = position_of(&pool, player_id).await;
    let terrain = movement::terrain(&state).await.unwrap();
    let tree = find_object(&terrain, spawn, "tree");
    teleport(&pool, player_id, tree.position).await;
    assert_eq!(
        harvest_request(&router, &token, &tree.id).await.status(),
        StatusCode::OK
    );
    cooldown_reset(&pool).await;
    assert_eq!(
        harvest_request(&router, &token, &tree.id).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        earned_ids(&achievements_of(&router, &token).await),
        ["first_harvest"]
    );

    // Jumping over several level thresholds earns each of them.
    sqlx::query("UPDATE players SET experience = $2 WHERE id = $1")
        .bind(player_id)
        .bind(crate::experience::experience_for(10) - 1)
        .execute(&pool)
        .await
        .unwrap();
    cooldown_reset(&pool).await;
    assert_eq!(
        harvest_request(&router, &token, &tree.id).await.status(),
        StatusCode::OK
    );
    let reply = achievements_of(&router, &token).await;
    let mut earned = earned_ids(&reply);
    earned.sort();
    assert_eq!(earned, ["first_harvest", "level_10", "level_5"]);
    let journeyman = reply["achievements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == "level_10")
        .unwrap();
    assert_eq!(journeyman["title"], "Journeyman");

    // The owner hears of each one once; another call does not repeat them.
    let mut announced = achievement_events(&router, &token).await;
    announced.sort();
    assert_eq!(announced, ["first_harvest", "level_10", "level_5"]);
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM player_events WHERE recipient_id = $1 AND kind = 'achievement'",
    )
    .bind(player_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 3);

    // A malformed id cannot be stored, whatever the code asks for.
    assert!(sqlx::query(
        "INSERT INTO player_achievements (player_id, achievement) VALUES ($1, 'Not valid')"
    )
    .bind(player_id)
    .execute(&pool)
    .await
    .is_err());

    // Without a session there is nothing to see.
    assert_eq!(
        player_request(&router, "GET", "/players/me/achievements", "", "bad-token")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}
