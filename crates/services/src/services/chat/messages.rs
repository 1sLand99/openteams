pub async fn create_message(
    pool: &SqlitePool,
    session_id: Uuid,
    sender_type: ChatSenderType,
    sender_id: Option<Uuid>,
    content: String,
    meta: Option<Value>,
) -> Result<ChatMessage, ChatServiceError> {
    create_message_with_id(
        pool,
        session_id,
        sender_type,
        sender_id,
        content,
        meta,
        Uuid::new_v4(),
    )
    .await
}

#[derive(Debug, Clone)]
pub struct IdempotentChatMessage {
    pub message: ChatMessage,
    pub created: bool,
}

#[derive(Debug, Clone)]
pub struct ResolvedChatMessageTarget {
    pub ordinal: i64,
    pub session_agent_id: Uuid,
    pub project_member_id: Option<Uuid>,
    pub agent_id: Uuid,
    pub member_name_snapshot: String,
    pub route_kind: ChatMessageTargetRouteKind,
    pub resolution_status: ChatMessageTargetResolutionStatus,
}

#[derive(Debug, Clone)]
pub struct IdempotentChatMessageDeliveryBundle {
    pub message: ChatMessage,
    pub targets: Vec<ChatMessageTarget>,
    pub deliveries: Vec<ChatMessageQueue>,
    /// Claimed, unbound deliveries that the runner may start only after commit.
    pub dispatch_deliveries: Vec<ChatMessageQueue>,
    pub runtime_revision: i64,
    pub created: bool,
}

pub(crate) fn normalized_client_message_id(
    meta: Option<&Value>,
) -> Result<Option<String>, ChatServiceError> {
    let Some(client_message_id) = meta
        .and_then(Value::as_object)
        .and_then(|meta| meta.get("client_message_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    if client_message_id.len() > 256 {
        return Err(ChatServiceError::Validation(
            "client_message_id cannot exceed 256 bytes".to_string(),
        ));
    }
    Ok(Some(client_message_id.to_string()))
}

async fn persist_delivery_bundle_rows(
    transaction: &mut Transaction<'_, Sqlite>,
    message: &ChatMessage,
    resolved_targets: &[ResolvedChatMessageTarget],
) -> Result<
    (
        Vec<ChatMessageTarget>,
        Vec<ChatMessageQueue>,
        Vec<ChatMessageQueue>,
    ),
    sqlx::Error,
> {
    let mut member_ids = Vec::with_capacity(resolved_targets.len());
    let mut seen_member_ids = HashSet::with_capacity(resolved_targets.len());

    for target in resolved_targets {
        let persisted = ChatMessageTarget::create_in_transaction(
            transaction,
            &CreateChatMessageTarget {
                message_id: message.id,
                ordinal: target.ordinal,
                session_id: message.session_id,
                session_agent_id: Some(target.session_agent_id),
                project_member_id: target.project_member_id,
                agent_id: target.agent_id,
                member_name_snapshot: target.member_name_snapshot.clone(),
                route_kind: target.route_kind,
                resolution_status: target.resolution_status,
            },
        )
        .await?;
        if persisted.session_agent_id != Some(target.session_agent_id) {
            return Err(sqlx::Error::Protocol(
                "persisted chat message target changed delivery identity".to_string(),
            ));
        }
        if target.resolution_status == ChatMessageTargetResolutionStatus::Resolved {
            ChatMessageQueue::create_queued_in_transaction(
                transaction,
                &CreateChatMessageQueue {
                    session_id: message.session_id,
                    session_agent_id: target.session_agent_id,
                    agent_id: target.agent_id,
                    chat_message_id: message.id,
                },
                Uuid::new_v4(),
            )
            .await?;
            if seen_member_ids.insert(target.session_agent_id) {
                member_ids.push(target.session_agent_id);
            }
        }
    }

    let mut dispatch_deliveries = Vec::with_capacity(member_ids.len());
    let mut dispatch_ids = HashSet::with_capacity(member_ids.len());
    for session_agent_id in member_ids {
        if let Some(claimed) =
            ChatMessageQueue::claim_next_in_transaction(transaction, session_agent_id).await?
            && dispatch_ids.insert(claimed.id)
        {
            dispatch_deliveries.push(claimed);
        }
    }

    let targets =
        ChatMessageTarget::find_by_message_in_transaction(transaction, message.id).await?;
    let deliveries =
        ChatMessageQueue::list_for_message_in_transaction(transaction, message.id).await?;
    for delivery in &deliveries {
        if matches!(
            delivery.status,
            QueuedMessageStatus::Starting | QueuedMessageStatus::Processing
        ) && delivery.run_id.is_none()
            && dispatch_ids.insert(delivery.id)
        {
            dispatch_deliveries.push(delivery.clone());
        }
    }

    Ok((targets, deliveries, dispatch_deliveries))
}

async fn load_message_delivery_bundle(
    pool: &SqlitePool,
    message: ChatMessage,
) -> Result<IdempotentChatMessageDeliveryBundle, ChatServiceError> {
    let targets = ChatMessageTarget::find_by_message(pool, message.id).await?;
    let deliveries = ChatMessageQueue::list_for_message(pool, message.id).await?;
    let dispatch_deliveries = deliveries
        .iter()
        .filter(|delivery| {
            matches!(
                delivery.status,
                QueuedMessageStatus::Starting | QueuedMessageStatus::Processing
            ) && delivery.run_id.is_none()
        })
        .cloned()
        .collect();
    let runtime_revision =
        ChatMessageQueue::current_runtime_revision(pool, message.session_id).await?;
    Ok(IdempotentChatMessageDeliveryBundle {
        message,
        targets,
        deliveries,
        dispatch_deliveries,
        runtime_revision,
        created: false,
    })
}

/// Persist a message and all of its resolved delivery targets in one transaction.
///
/// Queue triggers allocate the session runtime revision and outbox rows inside this transaction.
/// Claimed rows are returned to the runner but must not be dispatched until this function returns.
pub(crate) async fn create_message_delivery_bundle_with_id(
    pool: &SqlitePool,
    data: &CreateChatMessage,
    resolved_targets: &[ResolvedChatMessageTarget],
    client_message_id: Option<&str>,
    message_id: Uuid,
) -> Result<IdempotentChatMessageDeliveryBundle, ChatServiceError> {
    let mut transaction = pool.begin().await?;
    if let Some(client_message_id) = client_message_id {
        let claimed = ChatMessage::claim_idempotency_key_in_transaction(
            &mut transaction,
            data.session_id,
            client_message_id,
            message_id,
        )
        .await?;
        if !claimed {
            transaction.rollback().await?;
            let existing = ChatMessage::find_idempotent_user_message(
                pool,
                data.session_id,
                client_message_id,
            )
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;
            return load_message_delivery_bundle(pool, existing).await;
        }
    }

    let message = ChatMessage::create_in_transaction(&mut transaction, data, message_id).await?;
    let (targets, deliveries, dispatch_deliveries) =
        persist_delivery_bundle_rows(&mut transaction, &message, resolved_targets).await?;
    sqlx::query("UPDATE chat_sessions SET updated_at = datetime('now', 'subsec') WHERE id = ?1")
        .bind(data.session_id)
        .execute(&mut *transaction)
        .await?;
    let runtime_revision = ChatMessageQueue::current_runtime_revision_in_transaction(
        &mut transaction,
        data.session_id,
    )
    .await?;
    transaction.commit().await?;

    Ok(IdempotentChatMessageDeliveryBundle {
        message,
        targets,
        deliveries,
        dispatch_deliveries,
        runtime_revision,
        created: true,
    })
}

/// Fill target/delivery rows missing from messages written by the legacy split transaction path.
pub(crate) async fn ensure_message_delivery_bundle(
    pool: &SqlitePool,
    message: &ChatMessage,
    resolved_targets: &[ResolvedChatMessageTarget],
) -> Result<IdempotentChatMessageDeliveryBundle, ChatServiceError> {
    let mut transaction = pool.begin().await?;
    let (targets, deliveries, dispatch_deliveries) =
        persist_delivery_bundle_rows(&mut transaction, message, resolved_targets).await?;
    let runtime_revision = ChatMessageQueue::current_runtime_revision_in_transaction(
        &mut transaction,
        message.session_id,
    )
    .await?;
    transaction.commit().await?;
    Ok(IdempotentChatMessageDeliveryBundle {
        message: message.clone(),
        targets,
        deliveries,
        dispatch_deliveries,
        runtime_revision,
        created: false,
    })
}

/// Create a user message exactly once for a session/client key. A retry returns the original
/// row, allowing the route to skip analytics and runner dispatch on replay.
pub async fn create_message_idempotent(
    pool: &SqlitePool,
    session_id: Uuid,
    sender_type: ChatSenderType,
    sender_id: Option<Uuid>,
    content: String,
    meta: Option<Value>,
) -> Result<IdempotentChatMessage, ChatServiceError> {
    create_message_idempotent_with_id(
        pool,
        session_id,
        sender_type,
        sender_id,
        content,
        meta,
        Uuid::new_v4(),
    )
    .await
}

/// Attachment uploads reserve their storage path before persistence, so they provide the
/// candidate message id while retaining the same idempotency semantics.
pub async fn create_message_idempotent_with_id(
    pool: &SqlitePool,
    session_id: Uuid,
    sender_type: ChatSenderType,
    sender_id: Option<Uuid>,
    content: String,
    mut meta: Option<Value>,
    message_id: Uuid,
) -> Result<IdempotentChatMessage, ChatServiceError> {
    let client_message_id = if matches!(sender_type, ChatSenderType::User) {
        normalized_client_message_id(meta.as_ref())?
    } else {
        None
    };
    let Some(client_message_id) = client_message_id else {
        return Ok(IdempotentChatMessage {
            message: create_message_with_id(
                pool,
                session_id,
                sender_type,
                sender_id,
                content,
                meta,
                message_id,
            )
            .await?,
            created: true,
        });
    };

    if let Some(existing) =
        ChatMessage::find_idempotent_user_message(pool, session_id, &client_message_id).await?
    {
        return Ok(IdempotentChatMessage {
            message: existing,
            created: false,
        });
    }

    if let Some(meta_object) = meta.as_mut().and_then(Value::as_object_mut) {
        meta_object.insert(
            "client_message_id".to_string(),
            Value::String(client_message_id.clone()),
        );
    }
    let data = prepare_chat_message(
        pool,
        session_id,
        sender_type,
        sender_id,
        content,
        meta,
    )
    .await?;
    let mut transaction = pool.begin().await?;
    let claimed = ChatMessage::claim_idempotency_key_in_transaction(
        &mut transaction,
        session_id,
        &client_message_id,
        message_id,
    )
    .await?;
    if !claimed {
        transaction.rollback().await?;
        let existing = ChatMessage::find_idempotent_user_message(
            pool,
            session_id,
            &client_message_id,
        )
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
        return Ok(IdempotentChatMessage {
            message: existing,
            created: false,
        });
    }

    let message = ChatMessage::create_in_transaction(&mut transaction, &data, message_id).await?;
    sqlx::query("UPDATE chat_sessions SET updated_at = datetime('now', 'subsec') WHERE id = ?1")
        .bind(session_id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(IdempotentChatMessage {
        message,
        created: true,
    })
}

pub async fn create_message_with_id(
    pool: &SqlitePool,
    session_id: Uuid,
    sender_type: ChatSenderType,
    sender_id: Option<Uuid>,
    content: String,
    meta: Option<Value>,
    message_id: Uuid,
) -> Result<ChatMessage, ChatServiceError> {
    let data = prepare_chat_message(pool, session_id, sender_type, sender_id, content, meta).await?;

    let message = ChatMessage::create(pool, &data, message_id).await?;

    ChatSession::touch(pool, session_id).await?;

    Ok(message)
}

pub(crate) async fn prepare_chat_message(
    pool: &SqlitePool,
    session_id: Uuid,
    sender_type: ChatSenderType,
    sender_id: Option<Uuid>,
    content: String,
    meta: Option<Value>,
) -> Result<CreateChatMessage, ChatServiceError> {
    if matches!(sender_type, ChatSenderType::Agent) && sender_id.is_none() {
        return Err(ChatServiceError::Validation(
            "sender_id is required for agent messages".to_string(),
        ));
    }

    let session = ChatSession::find_by_id(pool, session_id)
        .await?
        .ok_or(ChatServiceError::SessionNotFound)?;

    if session.status != ChatSessionStatus::Active {
        return Err(ChatServiceError::SessionArchived);
    }

    let mut meta = meta.unwrap_or_else(|| serde_json::json!({}));
    if !meta.is_object() {
        meta = serde_json::json!({ "raw_meta": meta });
    }
    let mentions = match sender_type {
        ChatSenderType::Agent => parse_agent_send_mentions(&meta),
        ChatSenderType::User => parse_user_message_mentions(&content, &meta),
        _ => parse_mentions(&content),
    };
    if content.trim().is_empty() && !has_attachments(&meta) {
        return Err(ChatServiceError::Validation(
            "content cannot be empty".to_string(),
        ));
    }

    let sender_handle = meta
        .get("sender_handle")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string());
    let sender_name = if matches!(sender_type, ChatSenderType::Agent) {
        let sender_session_agent_id = meta
            .get("session_agent_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok());
        resolve_sender_member_name(pool, session_id, sender_session_agent_id, sender_id).await?
    } else {
        None
    };

    let sender_label = match sender_type {
        ChatSenderType::User => sender_handle.clone().unwrap_or_else(|| "user".to_string()),
        ChatSenderType::Agent => sender_name
            .clone()
            .or_else(|| sender_id.map(|id| id.to_string()))
            .unwrap_or_else(|| "agent".to_string()),
        ChatSenderType::System => "system".to_string(),
    };

    if meta.get("sender").is_none() {
        meta["sender"] = serde_json::json!({
            "type": sender_type,
            "id": sender_id,
            "handle": sender_handle,
            "name": sender_name,
            "label": sender_label,
        });
    }

    meta["structured"] = serde_json::json!({
        "sender_type": sender_type,
        "sender_id": sender_id,
        "sender_handle": sender_handle,
        "sender_label": sender_label,
        "content": content.clone(),
        "mentions": mentions.clone(),
        "created_at": Utc::now().to_rfc3339(),
    });

    Ok(CreateChatMessage {
        session_id,
        sender_type,
        sender_id,
        content,
        mentions,
        meta,
    })
}

pub fn is_protocol_notice_history_message(message: &ChatMessage) -> bool {
    matches!(message.sender_type, ChatSenderType::System)
        && message.meta.0.get("protocol_error").is_some()
}

pub fn should_include_message_in_history(message: &ChatMessage) -> bool {
    !is_protocol_notice_history_message(message)
}

pub async fn build_structured_messages(
    pool: &SqlitePool,
    session_id: Uuid,
) -> Result<Vec<Value>, ChatServiceError> {
    let messages = ChatMessage::find_by_session_id(pool, session_id, None)
        .await?
        .into_iter()
        .filter(should_include_message_in_history)
        .collect::<Vec<_>>();
    let member_names = member_name_overrides_for_session(pool, session_id).await?;
    let legacy_member_names =
        unambiguous_member_names_by_agent_for_session(pool, session_id).await?;
    let agents = ChatAgent::find_all(pool).await?;
    let agent_map: HashMap<Uuid, String> = agents
        .into_iter()
        .map(|agent| (agent.id, agent.name))
        .collect();

    let mut result = Vec::with_capacity(messages.len());

    for message in messages {
        let sender_handle = message
            .meta
            .0
            .get("sender_handle")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string());
        let sender_name = message
            .sender_session_agent_id
            .and_then(|id| member_names.get(&id).cloned())
            .or_else(|| {
                message
                    .sender_id
                    .and_then(|id| legacy_member_names.get(&id).cloned())
            })
            .or_else(|| message.sender_id.and_then(|id| agent_map.get(&id).cloned()));
        let sender_label = match message.sender_type {
            ChatSenderType::User => sender_handle.clone().unwrap_or_else(|| "user".to_string()),
            ChatSenderType::Agent => sender_name
                .clone()
                .or_else(|| message.sender_id.map(|id| id.to_string()))
                .unwrap_or_else(|| "agent".to_string()),
            ChatSenderType::System => "system".to_string(),
        };

        let sender = serde_json::json!({
            "type": message.sender_type,
            "id": message.sender_id,
            "handle": sender_handle,
            "name": sender_name,
            "label": sender_label,
        });

        result.push(serde_json::json!({
            "id": message.id,
            "session_id": message.session_id,
            "created_at": message.created_at,
            "sender": sender,
            "content": message.content,
            "mentions": message.mentions.0,
            "meta": message.meta.0,
        }));
    }

    Ok(result)
}
