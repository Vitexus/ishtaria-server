use super::*;

#[sqlx::test]
#[ignore = "requires DATABASE_URL pointing to PostgreSQL with CREATEDB permission"]
async fn a_trapper_sets_as_many_snares_as_he_carries_and_each_ruined_one_counts(pool: PgPool) {
    let _auth_test = AUTH_TESTS.acquire().await.unwrap();
    let world_id = initialize_spawn_world(&pool).await;
    let state = AppState {
        pool: pool.clone(),
        world_id,
    };
    let router = app(state.clone());
    let _token = create_session(&router, "trapper").await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM players")
        .fetch_one(&pool)
        .await
        .unwrap();
    let quest = "gilgamesh:the_hunter_and_the_wild_man";
    sqlx::query(
        "INSERT INTO player_quests (player_id, quest_id, stage) VALUES ($1, $2, 'lay_snares')",
    )
    .bind(id)
    .bind(quest)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO player_inventory (player_id, item_id, quantity) VALUES ($1, 'snare', 1) ON CONFLICT (player_id, item_id) DO UPDATE SET quantity = 1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    // The trapper stands far away; two snares are set and wildlife is at both.
    sqlx::query(
        "UPDATE players SET position_x = 6371000, position_y = 0, position_z = 0 WHERE id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    for z in [1000.0_f64, 1200.0] {
        let placed: i64 = sqlx::query_scalar(
            "INSERT INTO placed_objects (world_id, owner_id, kind, position_x, position_y, position_z) VALUES ($1, $2, 'snare', 0, 6371000, $3) RETURNING id",
        )
        .bind(world_id)
        .bind(id)
        .bind(z)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO hunting_snares (placed_id, owner_id, quest_id, stage, phase, animal_since) VALUES ($1, $2, $3, 'lay_snares', 'animal', now() - interval '1 hour')")
            .bind(placed)
            .bind(id)
            .bind(quest)
            .execute(&pool)
            .await
            .unwrap();
    }
    crate::hunting::tick(&state).await.unwrap();
    let stage: String = sqlx::query_scalar("SELECT stage FROM player_quests WHERE player_id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        stage, "snare_ruined_2",
        "each of the two ruined snares moved the quest on"
    );
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM hunting_snares")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
    let carried: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(quantity), 0)::bigint FROM player_inventory WHERE player_id = $1 AND item_id = 'snare'",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        carried, 1,
        "a trapper who still carries a snare gets no replacement"
    );
}
