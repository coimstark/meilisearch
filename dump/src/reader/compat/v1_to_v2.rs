use std::{collections::BTreeSet, str::FromStr};

use crate::reader::{v1, v2, Document};

use super::v2_to_v3::CompatV2ToV3;
use crate::Result;

pub struct CompatV1ToV2 {
    pub from: v1::V1Reader,
}

impl CompatV1ToV2 {
    pub fn new(v1: v1::V1Reader) -> Self {
        Self { from: v1 }
    }

    pub fn to_v3(self) -> CompatV2ToV3 {
        CompatV2ToV3::Compat(self)
    }

    pub fn version(&self) -> crate::Version {
        self.from.version()
    }

    pub fn date(&self) -> Option<time::OffsetDateTime> {
        self.from.date()
    }

    pub fn index_uuid(&self) -> Vec<v2::meta::IndexUuid> {
        self.from
            .index_uuid()
            .into_iter()
            .enumerate()
            // we use the index of the index 😬 as UUID for the index, so that we can link the v2::Task to their index
            .map(|(index, index_uuid)| v2::meta::IndexUuid {
                uid: index_uuid.uid,
                uuid: uuid::Uuid::from_u128(index as u128),
            })
            .collect()
    }

    pub fn indexes(&self) -> Result<impl Iterator<Item = Result<CompatIndexV1ToV2>> + '_> {
        Ok(self.from.indexes()?.map(|index_reader| Ok(CompatIndexV1ToV2 { from: index_reader? })))
    }

    pub fn tasks(
        &mut self,
    ) -> Box<dyn Iterator<Item = Result<(v2::Task, Option<v2::UpdateFile>)>> + '_> {
        // Convert an error here to an iterator yielding the error
        let indexes = match self.from.indexes() {
            Ok(indexes) => indexes,
            Err(err) => return Box::new(std::iter::once(Err(err))),
        };
        let it = indexes.enumerate().flat_map(
            move |(index, index_reader)| -> Box<dyn Iterator<Item = _>> {
                let index_reader = match index_reader {
                    Ok(index_reader) => index_reader,
                    Err(err) => return Box::new(std::iter::once(Err(err))),
                };
                Box::new(
                    index_reader
                        .tasks()
                        // Filter out the UpdateStatus::Customs variant that is not supported in v2
                        // and enqueued tasks, that don't contain the necessary update file in v1
                        .filter_map(move |task| -> Option<_> {
                            let task = match task {
                                Ok(task) => task,
                                Err(err) => return Some(Err(err)),
                            };
                            Some(Ok((
                                v2::Task {
                                    uuid: uuid::Uuid::from_u128(index as u128),
                                    update: Option::from(task)?,
                                },
                                None,
                            )))
                        }),
                )
            },
        );
        Box::new(it)
    }
}

pub struct CompatIndexV1ToV2 {
    pub from: v1::V1IndexReader,
}

impl CompatIndexV1ToV2 {
    pub fn metadata(&self) -> &crate::IndexMetadata {
        self.from.metadata()
    }

    pub fn documents(&mut self) -> Result<Box<dyn Iterator<Item = Result<Document>> + '_>> {
        self.from.documents().map(|it| Box::new(it) as Box<dyn Iterator<Item = _>>)
    }

    pub fn settings(&mut self) -> Result<v2::settings::Settings<v2::settings::Checked>> {
        Ok(v2::settings::Settings::<v2::settings::Unchecked>::from(self.from.settings()?).check())
    }
}

impl From<v1::settings::Settings> for v2::Settings<v2::Unchecked> {
    fn from(source: v1::settings::Settings) -> Self {
        let displayed_attributes = source
            .displayed_attributes
            .map(|opt| opt.map(|displayed_attributes| displayed_attributes.into_iter().collect()));
        let attributes_for_faceting = source.attributes_for_faceting.map(|opt| {
            opt.map(|attributes_for_faceting| attributes_for_faceting.into_iter().collect())
        });
        let ranking_rules = source.ranking_rules.map(|opt| {
            opt.map(|ranking_rules| {
                ranking_rules
                    .into_iter()
                    .filter_map(|ranking_rule| {
                        match v1::settings::RankingRule::from_str(&ranking_rule) {
                            Ok(ranking_rule) => {
                                let criterion: Option<v2::settings::Criterion> =
                                    ranking_rule.into();
                                criterion.as_ref().map(ToString::to_string)
                            }
                            Err(()) => Some(ranking_rule),
                        }
                    })
                    .collect()
            })
        });

        Self {
            displayed_attributes,
            searchable_attributes: source.searchable_attributes,
            filterable_attributes: attributes_for_faceting,
            ranking_rules,
            stop_words: source.stop_words,
            synonyms: source.synonyms,
            distinct_attribute: source.distinct_attribute,
            _kind: std::marker::PhantomData,
        }
    }
}

impl From<v1::update::UpdateStatus> for Option<v2::updates::UpdateStatus> {
    fn from(source: v1::update::UpdateStatus) -> Self {
        use v1::update::UpdateStatus as UpdateStatusV1;
        use v2::updates::UpdateStatus as UpdateStatusV2;
        Some(match source {
            UpdateStatusV1::Enqueued { content } => {
                log::warn!(
                    "Cannot import task {} (importing enqueued tasks from v1 dumps is unsupported)",
                    content.update_id
                );
                log::warn!("Task will be skipped in the queue of imported tasks.");

                return None;
            }
            UpdateStatusV1::Failed { content } => UpdateStatusV2::Failed(v2::updates::Failed {
                from: v2::updates::Processing {
                    from: v2::updates::Enqueued {
                        update_id: content.update_id,
                        meta: Option::from(content.update_type)?,
                        enqueued_at: content.enqueued_at,
                        content: None,
                    },
                    started_processing_at: content.processed_at
                        - std::time::Duration::from_secs_f64(content.duration),
                },
                error: v2::ResponseError {
                    // error code is ignored by serialization, and so always default in deserialized v2 dumps
                    // that's a good thing, because we don't have them in v1 dump 😅
                    code: http::StatusCode::default(),
                    message: content.error.unwrap_or_default(),
                    // error codes are unchanged between v1 and v2
                    error_code: content.error_code.unwrap_or_default(),
                    // error types are unchanged between v1 and v2
                    error_type: content.error_type.unwrap_or_default(),
                    // error links are unchanged between v1 and v2
                    error_link: content.error_link.unwrap_or_default(),
                },
                failed_at: content.processed_at,
            }),
            UpdateStatusV1::Processed { content } => {
                UpdateStatusV2::Processed(v2::updates::Processed {
                    success: match &content.update_type {
                        v1::update::UpdateType::ClearAll => {
                            v2::updates::UpdateResult::DocumentDeletion { deleted: u64::MAX }
                        }
                        v1::update::UpdateType::Customs => v2::updates::UpdateResult::Other,
                        v1::update::UpdateType::DocumentsAddition { number } => {
                            v2::updates::UpdateResult::DocumentsAddition(
                                v2::updates::DocumentAdditionResult { nb_documents: *number },
                            )
                        }
                        v1::update::UpdateType::DocumentsPartial { number } => {
                            v2::updates::UpdateResult::DocumentsAddition(
                                v2::updates::DocumentAdditionResult { nb_documents: *number },
                            )
                        }
                        v1::update::UpdateType::DocumentsDeletion { number } => {
                            v2::updates::UpdateResult::DocumentDeletion { deleted: *number as u64 }
                        }
                        v1::update::UpdateType::Settings { .. } => v2::updates::UpdateResult::Other,
                    },
                    processed_at: content.processed_at,
                    from: v2::updates::Processing {
                        from: v2::updates::Enqueued {
                            update_id: content.update_id,
                            meta: Option::from(content.update_type)?,
                            enqueued_at: content.enqueued_at,
                            content: None,
                        },
                        started_processing_at: content.processed_at
                            - std::time::Duration::from_secs_f64(content.duration),
                    },
                })
            }
        })
    }
}

impl From<v1::update::UpdateType> for Option<v2::updates::UpdateMeta> {
    fn from(source: v1::update::UpdateType) -> Self {
        Some(match source {
            v1::update::UpdateType::ClearAll => v2::updates::UpdateMeta::ClearDocuments,
            v1::update::UpdateType::Customs => {
                log::warn!("Ignoring task with type 'Customs' that is no longer supported");
                return None;
            }
            v1::update::UpdateType::DocumentsAddition { .. } => {
                v2::updates::UpdateMeta::DocumentsAddition {
                    method: v2::updates::IndexDocumentsMethod::ReplaceDocuments,
                    format: v2::updates::UpdateFormat::Json,
                    primary_key: None,
                }
            }
            v1::update::UpdateType::DocumentsPartial { .. } => {
                v2::updates::UpdateMeta::DocumentsAddition {
                    method: v2::updates::IndexDocumentsMethod::UpdateDocuments,
                    format: v2::updates::UpdateFormat::Json,
                    primary_key: None,
                }
            }
            v1::update::UpdateType::DocumentsDeletion { .. } => {
                v2::updates::UpdateMeta::DeleteDocuments { ids: vec![] }
            }
            v1::update::UpdateType::Settings { settings } => {
                v2::updates::UpdateMeta::Settings((*settings).into())
            }
        })
    }
}

impl From<v1::settings::SettingsUpdate> for v2::Settings<v2::Unchecked> {
    fn from(source: v1::settings::SettingsUpdate) -> Self {
        let displayed_attributes: Option<Option<BTreeSet<String>>> =
            source.displayed_attributes.into();

        let attributes_for_faceting: Option<Option<Vec<String>>> =
            source.attributes_for_faceting.into();

        let ranking_rules: Option<Option<Vec<v1::settings::RankingRule>>> =
            source.ranking_rules.into();

        // go from the concrete types of v1 (RankingRule) to the concrete type of v2 (Criterion),
        // and then back to string as this is what the settings manipulate
        let ranking_rules = ranking_rules.map(|opt| {
            opt.map(|ranking_rules| {
                ranking_rules
                    .into_iter()
                    // filter out the WordsPosition ranking rule that exists in v1 but not v2
                    .filter_map(|ranking_rule| {
                        Option::<v2::settings::Criterion>::from(ranking_rule)
                    })
                    .map(|criterion| criterion.to_string())
                    .collect()
            })
        });

        Self {
            displayed_attributes: displayed_attributes.map(|opt| {
                opt.map(|displayed_attributes| displayed_attributes.into_iter().collect())
            }),
            searchable_attributes: source.searchable_attributes.into(),
            filterable_attributes: attributes_for_faceting.map(|opt| {
                opt.map(|attributes_for_faceting| attributes_for_faceting.into_iter().collect())
            }),
            ranking_rules,
            stop_words: source.stop_words.into(),
            synonyms: source.synonyms.into(),
            distinct_attribute: source.distinct_attribute.into(),
            _kind: std::marker::PhantomData,
        }
    }
}

impl From<v1::settings::RankingRule> for Option<v2::settings::Criterion> {
    fn from(source: v1::settings::RankingRule) -> Self {
        match source {
            v1::settings::RankingRule::Typo => Some(v2::settings::Criterion::Typo),
            v1::settings::RankingRule::Words => Some(v2::settings::Criterion::Words),
            v1::settings::RankingRule::Proximity => Some(v2::settings::Criterion::Proximity),
            v1::settings::RankingRule::Attribute => Some(v2::settings::Criterion::Attribute),
            v1::settings::RankingRule::WordsPosition => {
                log::warn!("Removing the 'WordsPosition' ranking rule that is no longer supported, please check the resulting ranking rules of your indexes");
                None
            }
            v1::settings::RankingRule::Exactness => Some(v2::settings::Criterion::Exactness),
            v1::settings::RankingRule::Asc(field_name) => {
                Some(v2::settings::Criterion::Asc(field_name))
            }
            v1::settings::RankingRule::Desc(field_name) => {
                Some(v2::settings::Criterion::Desc(field_name))
            }
        }
    }
}

impl<T> From<v1::settings::UpdateState<T>> for Option<Option<T>> {
    fn from(source: v1::settings::UpdateState<T>) -> Self {
        match source {
            v1::settings::UpdateState::Update(new_value) => Some(Some(new_value)),
            v1::settings::UpdateState::Clear => Some(None),
            v1::settings::UpdateState::Nothing => None,
        }
    }
}
