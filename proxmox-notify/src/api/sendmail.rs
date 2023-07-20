use crate::api::ApiError;
use crate::endpoints::sendmail::{
    DeleteableSendmailProperty, SendmailConfig, SendmailConfigUpdater, SENDMAIL_TYPENAME,
};
use crate::Config;

/// Get a list of all sendmail endpoints.
///
/// The caller is responsible for any needed permission checks.
/// Returns a list of all sendmail endpoints or an `ApiError` if the config is erroneous.
pub fn get_endpoints(config: &Config) -> Result<Vec<SendmailConfig>, ApiError> {
    config
        .config
        .convert_to_typed_array(SENDMAIL_TYPENAME)
        .map_err(|e| ApiError::internal_server_error("Could not fetch endpoints", Some(e.into())))
}

/// Get sendmail endpoint with given `name`.
///
/// The caller is responsible for any needed permission checks.
/// Returns the endpoint or an `ApiError` if the endpoint was not found.
pub fn get_endpoint(config: &Config, name: &str) -> Result<SendmailConfig, ApiError> {
    config
        .config
        .lookup(SENDMAIL_TYPENAME, name)
        .map_err(|_| ApiError::not_found(format!("endpoint '{name}' not found"), None))
}

/// Add a new sendmail endpoint.
///
/// The caller is responsible for any needed permission checks.
/// The caller also responsible for locking the configuration files.
/// Returns an `ApiError` if an endpoint with the same name already exists,
/// or if the endpoint could not be saved.
pub fn add_endpoint(config: &mut Config, endpoint: &SendmailConfig) -> Result<(), ApiError> {
    if super::endpoint_exists(config, &endpoint.name) {
        return Err(ApiError::bad_request(
            format!("endpoint with name '{}' already exists!", &endpoint.name),
            None,
        ));
    }

    config
        .config
        .set_data(&endpoint.name, SENDMAIL_TYPENAME, endpoint)
        .map_err(|e| {
            ApiError::internal_server_error(
                format!("could not save endpoint '{}'", endpoint.name),
                Some(e.into()),
            )
        })?;

    Ok(())
}

/// Update existing sendmail endpoint
///
/// The caller is responsible for any needed permission checks.
/// The caller also responsible for locking the configuration files.
/// Returns an `ApiError` if the config could not be saved.
pub fn update_endpoint(
    config: &mut Config,
    name: &str,
    updater: &SendmailConfigUpdater,
    delete: Option<&[DeleteableSendmailProperty]>,
    digest: Option<&[u8]>,
) -> Result<(), ApiError> {
    super::verify_digest(config, digest)?;

    let mut endpoint = get_endpoint(config, name)?;

    if let Some(delete) = delete {
        for deleteable_property in delete {
            match deleteable_property {
                DeleteableSendmailProperty::FromAddress => endpoint.from_address = None,
                DeleteableSendmailProperty::Author => endpoint.author = None,
                DeleteableSendmailProperty::Comment => endpoint.comment = None,
            }
        }
    }

    if let Some(mailto) = &updater.mailto {
        endpoint.mailto = mailto.iter().map(String::from).collect();
    }

    if let Some(from_address) = &updater.from_address {
        endpoint.from_address = Some(from_address.into());
    }

    if let Some(author) = &updater.author {
        endpoint.author = Some(author.into());
    }

    if let Some(comment) = &updater.comment {
        endpoint.comment = Some(comment.into());
    }

    config
        .config
        .set_data(name, SENDMAIL_TYPENAME, &endpoint)
        .map_err(|e| {
            ApiError::internal_server_error(
                format!("could not save endpoint '{name}'"),
                Some(e.into()),
            )
        })?;

    Ok(())
}

/// Delete existing sendmail endpoint
///
/// The caller is responsible for any needed permission checks.
/// The caller also responsible for locking the configuration files.
/// Returns an `ApiError` if the endpoint does not exist.
pub fn delete_endpoint(config: &mut Config, name: &str) -> Result<(), ApiError> {
    // Check if the endpoint exists
    let _ = get_endpoint(config, name)?;

    config.config.sections.remove(name);

    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::api::test_helpers::*;

    pub fn add_sendmail_endpoint_for_test(config: &mut Config, name: &str) -> Result<(), ApiError> {
        add_endpoint(
            config,
            &SendmailConfig {
                name: name.into(),
                mailto: vec!["user1@example.com".into()],
                from_address: Some("from@example.com".into()),
                author: Some("root".into()),
                comment: Some("Comment".into()),
            },
        )?;

        assert!(get_endpoint(config, name).is_ok());
        Ok(())
    }

    #[test]
    fn test_sendmail_create() -> Result<(), ApiError> {
        let mut config = empty_config();

        assert_eq!(get_endpoints(&config)?.len(), 0);
        add_sendmail_endpoint_for_test(&mut config, "sendmail-endpoint")?;

        // Endpoints must have a unique name
        assert!(add_sendmail_endpoint_for_test(&mut config, "sendmail-endpoint").is_err());
        assert_eq!(get_endpoints(&config)?.len(), 1);
        Ok(())
    }

    #[test]
    fn test_update_not_existing_returns_error() -> Result<(), ApiError> {
        let mut config = empty_config();

        assert!(update_endpoint(&mut config, "test", &Default::default(), None, None,).is_err());

        Ok(())
    }

    #[test]
    fn test_update_invalid_digest_returns_error() -> Result<(), ApiError> {
        let mut config = empty_config();
        add_sendmail_endpoint_for_test(&mut config, "sendmail-endpoint")?;

        assert!(update_endpoint(
            &mut config,
            "sendmail-endpoint",
            &SendmailConfigUpdater {
                mailto: Some(vec!["user2@example.com".into(), "user3@example.com".into()]),
                from_address: Some("root@example.com".into()),
                author: Some("newauthor".into()),
                comment: Some("new comment".into()),
            },
            None,
            Some(&[0; 32]),
        )
        .is_err());

        Ok(())
    }

    #[test]
    fn test_sendmail_update() -> Result<(), ApiError> {
        let mut config = empty_config();
        add_sendmail_endpoint_for_test(&mut config, "sendmail-endpoint")?;

        let digest = config.digest;

        update_endpoint(
            &mut config,
            "sendmail-endpoint",
            &SendmailConfigUpdater {
                mailto: Some(vec!["user2@example.com".into(), "user3@example.com".into()]),
                from_address: Some("root@example.com".into()),
                author: Some("newauthor".into()),
                comment: Some("new comment".into()),
            },
            None,
            Some(&digest),
        )?;

        let endpoint = get_endpoint(&config, "sendmail-endpoint")?;

        assert_eq!(
            endpoint.mailto,
            vec![
                "user2@example.com".to_string(),
                "user3@example.com".to_string()
            ]
        );
        assert_eq!(endpoint.from_address, Some("root@example.com".to_string()));
        assert_eq!(endpoint.author, Some("newauthor".to_string()));
        assert_eq!(endpoint.comment, Some("new comment".to_string()));

        // Test property deletion
        update_endpoint(
            &mut config,
            "sendmail-endpoint",
            &Default::default(),
            Some(&[
                DeleteableSendmailProperty::FromAddress,
                DeleteableSendmailProperty::Author,
            ]),
            None,
        )?;

        let endpoint = get_endpoint(&config, "sendmail-endpoint")?;

        assert_eq!(endpoint.from_address, None);
        assert_eq!(endpoint.author, None);

        Ok(())
    }

    #[test]
    fn test_sendmail_delete() -> Result<(), ApiError> {
        let mut config = empty_config();
        add_sendmail_endpoint_for_test(&mut config, "sendmail-endpoint")?;

        delete_endpoint(&mut config, "sendmail-endpoint")?;
        assert!(delete_endpoint(&mut config, "sendmail-endpoint").is_err());
        assert_eq!(get_endpoints(&config)?.len(), 0);

        Ok(())
    }
}