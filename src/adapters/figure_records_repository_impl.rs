use std::str::FromStr;

use anyhow::{anyhow, ensure, Context};
use sqlx::{Acquire, Postgres};
use ulid::Ulid;

use crate::{entities, ports};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
struct FigureRecordModel {
    id: String,
    user_id: String,
    character: String,
    figure: serde_json::Value,
    created_at: DateTime<Utc>,
    stroke_count: i32,
    disabled: bool,
    version: i32,
}

impl FigureRecordModel {
    pub fn into_entity(self) -> anyhow::Result<entities::FigureRecord> {
        let id = Ulid::from_str(&self.id).context("ulid decode error")?;

        let character = entities::Character::try_from(self.character.as_str())?;

        let figure = entities::Figure::from_json_ast(self.figure)
            .ok_or_else(|| anyhow!("figure must be valid json"))?;

        ensure!(
            self.stroke_count == i32::from(figure.stroke_count()),
            "stroke_count invalid"
        );

        Ok(entities::FigureRecord {
            id: entities::FigureRecordId::from(id),
            user_id: entities::UserId::from(self.user_id),
            character,
            figure,
            created_at: self.created_at,
            disabled: self.disabled,
            version: entities::Version::try_from(self.version)?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct FigureRecordsRepositoryImpl<A> {
    db: A,
}

impl<A> FigureRecordsRepositoryImpl<A> {
    pub fn new(db: A) -> Self {
        Self { db }
    }
}

impl<A> ports::FigureRecordsRepository for FigureRecordsRepositoryImpl<A>
where
    A: Send,
    for<'c> &'c A: Acquire<'c, Database = Postgres>,
{
    type Error = anyhow::Error;

    async fn create(
        &mut self,
        user_id: entities::UserId,
        now: DateTime<Utc>,
        character: entities::Character,
        figure: entities::Figure,
    ) -> Result<entities::FigureRecord, Self::Error> {
        let mut trx = self.db.begin().await?;
        let record = entities::FigureRecord {
            id: entities::FigureRecordId::from(Ulid::from_datetime(now)),
            user_id,
            character,
            figure,
            created_at: now,
            disabled: false,
            version: entities::Version::new(),
        };

        sqlx::query!(
            r#"
                INSERT INTO figure_records (id, user_id, character, figure, created_at, stroke_count)
                VALUES ($1, $2, $3, $4, $5, $6)
            "#,
            Ulid::from(record.id).to_string(),
            String::from(record.user_id.clone()),
            String::from(record.character.clone()),
            record.figure.to_json_ast(),
            record.created_at,
            i32::from(record.figure.stroke_count()),
        )
        .execute(&mut *trx)
        .await
        .context("fetch figure_records")?;

        trx.commit().await?;
        Ok(record)
    }

    async fn update(
        &mut self,
        mut figure_record: entities::FigureRecord,
        disabled: Option<bool>,
    ) -> Result<entities::FigureRecord, Self::Error> {
        let mut trx = self.db.begin().await?;
        let prev_version = figure_record.version;

        figure_record.version = figure_record.version.next();
        if let Some(disabled) = disabled {
            figure_record.disabled = disabled;
        }

        let result = sqlx::query!(
            r#"
            UPDATE figure_records
                SET
                    disabled = $1,
                    version = $2
                WHERE
                    user_id = $3
                    AND
                    id = $4
                    AND
                    version = $5
            "#,
            disabled,
            i32::from(figure_record.version),
            String::from(figure_record.user_id.clone()),
            Ulid::from(figure_record.id.clone()).to_string(),
            i32::from(prev_version),
        )
        .execute(&mut *trx)
        .await
        .context("update figure_record")?;

        if result.rows_affected() == 0 {
            return Err(anyhow!("conflict"));
        }

        trx.commit().await?;
        Ok(figure_record)
    }

    async fn get_by_ids(
        &mut self,
        user_id: entities::UserId,
        ids: &[entities::FigureRecordId],
    ) -> Result<Vec<entities::FigureRecord>, Self::Error> {
        let mut conn = self.db.acquire().await?;
        let ids = ids
            .iter()
            .map(|&id| Ulid::from(id).to_string())
            .collect::<Vec<_>>();

        let models = sqlx::query_as!(
            FigureRecordModel,
            r#"
                SELECT
                    r.id,
                    r.user_id,
                    r.character,
                    r.figure,
                    r.created_at,
                    r.stroke_count,
                    r.disabled,
                    r.version
                FROM
                    figure_records AS r
                    LEFT OUTER JOIN user_configs ON r.user_id = user_configs.user_id
                WHERE
                    r.id = Any($1)
                    AND (r.user_id = $2 OR user_configs.allow_sharing_figure_records)
            "#,
            ids.as_slice(),
            String::from(user_id.clone()),
        )
        .fetch_all(&mut *conn)
        .await
        .context("fetch figure_records")?;

        let figure_records = models
            .into_iter()
            .map(|model| model.into_entity())
            .collect::<anyhow::Result<Vec<_>>>()
            .context("convert FigureRecord")?;

        Ok(figure_records)
    }

    async fn get_by_character_config_ids(
        &mut self,
        user_id: entities::UserId,
        character_config_ids: &[(entities::Character, entities::StrokeCount)],
        ids: Option<&[entities::FigureRecordId]>,
        after_id: Option<entities::FigureRecordId>,
        before_id: Option<entities::FigureRecordId>,
        limit_per_character: entities::Limit,
        user_type: Option<ports::UserType>,
    ) -> Result<Vec<entities::FigureRecord>, Self::Error> {
        let mut conn = self.db.acquire().await?;

        let ids = ids.as_ref().map(|ids| {
            ids.iter()
                .map(|&id| Ulid::from(id).to_string())
                .collect::<Vec<_>>()
        });

        let character_values = character_config_ids
            .iter()
            .map(|(character, _)| String::from(character.clone()))
            .collect::<Vec<_>>();

        let stroke_count_values = character_config_ids
            .iter()
            .map(|(_, stroke_count)| i32::from(*stroke_count))
            .collect::<Vec<_>>();

        let models = sqlx::query_as!(
            FigureRecordModel,
            r#"
                    WITH inputs AS (
                        SELECT character, stroke_count
                        FROM unnest($1::VARCHAR(8)[], $2::INTEGER[]) AS t(character, stroke_count)
                    )
                    SELECT
                        id,
                        user_id,
                        character,
                        figure,
                        created_at,
                        stroke_count,
                        disabled,
                        version
                    FROM (
                        SELECT
                            r.id,
                            r.user_id,
                            r.character,
                            r.figure,
                            r.created_at,
                            r.stroke_count,
                            rank() OVER (
                                PARTITION BY r.character, r.stroke_count
                                ORDER BY
                                    CASE WHEN $7 = 0 THEN r.id END DESC,
                                    CASE WHEN $7 = 1 THEN r.id END ASC
                            ) AS rank,
                            r.disabled,
                            r.version
                        FROM
                            figure_records AS r
                        JOIN
                            inputs ON r.character = inputs.character
                            AND
                            r.stroke_count = inputs.stroke_count
                        LEFT OUTER JOIN user_configs ON r.user_id = user_configs.user_id
                        WHERE
                            (r.user_id = $3 OR user_configs.allow_sharing_figure_records)
                            AND
                            ($4::VARCHAR(64)[] IS NULL OR r.id = Any($4))
                            AND
                            ($5::VARCHAR(64) IS NULL OR r.id < $5)
                            AND
                            ($6::VARCHAR(64) IS NULL OR r.id > $6)
                            AND
                            (NOT $9 OR r.user_id = $3)
                            AND
                            (NOT $10 OR r.user_id <> $3)
                            AND
                            NOT r.disabled
                    ) as r
                    WHERE
                        rank <= $8
                    ORDER BY
                        id DESC
                "#,
            character_values.as_slice(),
            stroke_count_values.as_slice(),
            String::from(user_id.clone()),
            ids.as_ref().map(|ids| ids.as_slice()),
            after_id.map(|id| Ulid::from(id).to_string()),
            before_id.map(|id| Ulid::from(id).to_string()),
            i32::from(limit_per_character.kind() == entities::LimitKind::Last),
            i64::from(limit_per_character.value()),
            user_type == Some(ports::UserType::Myself),
            user_type == Some(ports::UserType::Other),
        )
        .fetch_all(&mut *conn)
        .await
        .context("fetch figure_records")?;

        let figure_records = models
            .into_iter()
            .map(|model| model.into_entity())
            .collect::<anyhow::Result<Vec<_>>>()
            .context("convert FigureRecord")?;

        Ok(figure_records)
    }
}
