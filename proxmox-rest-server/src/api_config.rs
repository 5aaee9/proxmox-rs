use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::{format_err, Error};
use hyper::http::request::Parts;
use hyper::{Body, Response};

use proxmox_router::{Router, RpcEnvironmentType, UserInformation};
use proxmox_sys::fs::{create_path, CreateOptions};

use crate::rest::Handler;
use crate::{AuthError, CommandSocket, FileLogOptions, FileLogger, RestEnvironment, ServerAdapter};

/// REST server configuration
pub struct ApiConfig {
    basedir: PathBuf,
    aliases: HashMap<String, PathBuf>,
    env_type: RpcEnvironmentType,
    request_log: Option<Arc<Mutex<FileLogger>>>,
    auth_log: Option<Arc<Mutex<FileLogger>>>,
    adapter: Pin<Box<dyn ServerAdapter + Send + Sync>>,
    handlers: Vec<Handler>,

    #[cfg(feature = "templates")]
    templates: templates::Templates,
}

impl ApiConfig {
    /// Creates a new instance
    ///
    /// `basedir` - File lookups are relative to this directory.
    ///
    /// `env_type` - The environment type.
    ///
    /// `api_auth` - The Authentication handler
    ///
    /// `get_index_fn` - callback to generate the root page
    /// (index). Please note that this functions gets a reference to
    /// the [ApiConfig], so it can use [Handlebars] templates
    /// ([render_template](Self::render_template) to generate pages.
    pub fn new<B: Into<PathBuf>>(basedir: B, env_type: RpcEnvironmentType) -> Self {
        Self {
            basedir: basedir.into(),
            aliases: HashMap::new(),
            env_type,
            request_log: None,
            auth_log: None,
            adapter: Box::pin(DummyAdapter),
            handlers: Vec::new(),

            #[cfg(feature = "templates")]
            templates: Default::default(),
        }
    }

    pub(crate) async fn get_index(
        &self,
        rest_env: RestEnvironment,
        parts: Parts,
    ) -> Response<Body> {
        self.adapter.get_index(rest_env, parts).await
    }

    pub(crate) async fn check_auth(
        &self,
        headers: &http::HeaderMap,
        method: &hyper::Method,
    ) -> Result<(String, Box<dyn UserInformation + Sync + Send>), AuthError> {
        self.adapter.check_auth(headers, method).await
    }

    pub(crate) fn find_alias(&self, mut components: &[&str]) -> PathBuf {
        let mut filename = self.basedir.clone();
        if components.is_empty() {
            return filename;
        }

        if let Some(subdir) = self.aliases.get(components[0]) {
            filename.push(subdir);
            components = &components[1..];
        }

        filename.extend(components);

        filename
    }

    /// Register a path alias
    ///
    /// This can be used to redirect file lookups to a specific
    /// directory, e.g.:
    ///
    /// ```
    /// use proxmox_rest_server::ApiConfig;
    /// // let mut config = ApiConfig::new(...);
    /// # fn fake(config: &mut ApiConfig) {
    /// config.add_alias("extjs", "/usr/share/javascript/extjs");
    /// # }
    /// ```
    pub fn add_alias<S, P>(&mut self, alias: S, path: P)
    where
        S: Into<String>,
        P: Into<PathBuf>,
    {
        self.aliases.insert(alias.into(), path.into());
    }

    pub(crate) fn env_type(&self) -> RpcEnvironmentType {
        self.env_type
    }

    /// Register a [Handlebars] template file
    ///
    /// Those templates cane be use with [render_template](Self::render_template) to generate pages.
    #[cfg(feature = "templates")]
    pub fn register_template<P>(&self, name: &str, path: P) -> Result<(), Error>
    where
        P: Into<PathBuf>,
    {
        self.templates.register(name, path)
    }

    /// Checks if the template was modified since the last rendering
    /// if yes, it loads a the new version of the template
    #[cfg(feature = "templates")]
    pub fn render_template<T>(&self, name: &str, data: &T) -> Result<String, Error>
    where
        T: serde::Serialize,
    {
        self.templates.render(name, data)
    }

    /// Enable the access log feature
    ///
    /// When enabled, all requests are logged to the specified file.
    /// This function also registers a `api-access-log-reopen`
    /// command one the [CommandSocket].
    pub fn enable_access_log<P>(
        &mut self,
        path: P,
        dir_opts: Option<CreateOptions>,
        file_opts: Option<CreateOptions>,
        commando_sock: &mut CommandSocket,
    ) -> Result<(), Error>
    where
        P: Into<PathBuf>,
    {
        let path: PathBuf = path.into();
        if let Some(base) = path.parent() {
            if !base.exists() {
                create_path(base, None, dir_opts).map_err(|err| format_err!("{}", err))?;
            }
        }

        let logger_options = FileLogOptions {
            append: true,
            file_opts: file_opts.unwrap_or_default(),
            ..Default::default()
        };
        let request_log = Arc::new(Mutex::new(FileLogger::new(&path, logger_options)?));
        self.request_log = Some(Arc::clone(&request_log));

        commando_sock.register_command("api-access-log-reopen".into(), move |_args| {
            log::info!("re-opening access-log file");
            request_log.lock().unwrap().reopen()?;
            Ok(serde_json::Value::Null)
        })?;

        Ok(())
    }

    /// Enable the authentication log feature
    ///
    /// When enabled, all authentication requests are logged to the
    /// specified file. This function also registers a
    /// `api-auth-log-reopen` command one the [CommandSocket].
    pub fn enable_auth_log<P>(
        &mut self,
        path: P,
        dir_opts: Option<CreateOptions>,
        file_opts: Option<CreateOptions>,
        commando_sock: &mut CommandSocket,
    ) -> Result<(), Error>
    where
        P: Into<PathBuf>,
    {
        let path: PathBuf = path.into();
        if let Some(base) = path.parent() {
            if !base.exists() {
                create_path(base, None, dir_opts).map_err(|err| format_err!("{}", err))?;
            }
        }

        let logger_options = FileLogOptions {
            append: true,
            prefix_time: true,
            file_opts: file_opts.unwrap_or_default(),
            ..Default::default()
        };
        let auth_log = Arc::new(Mutex::new(FileLogger::new(&path, logger_options)?));
        self.auth_log = Some(Arc::clone(&auth_log));

        commando_sock.register_command("api-auth-log-reopen".into(), move |_args| {
            log::info!("re-opening auth-log file");
            auth_log.lock().unwrap().reopen()?;
            Ok(serde_json::Value::Null)
        })?;

        Ok(())
    }

    pub(crate) fn get_access_log(&self) -> Option<&Arc<Mutex<FileLogger>>> {
        self.request_log.as_ref()
    }

    pub(crate) fn get_auth_log(&self) -> Option<&Arc<Mutex<FileLogger>>> {
        self.auth_log.as_ref()
    }

    pub(crate) fn find_handler<'a>(&'a self, path_components: &[&str]) -> Option<&'a Handler> {
        self.handlers
            .iter()
            .find(|handler| path_components.strip_prefix(handler.prefix).is_some())
    }

    pub fn add_default_api2_handler(&mut self, router: &'static Router) -> &mut Self {
        self.handlers.push(Handler::default_api2_handler(router));
        self
    }

    pub fn add_formatted_router(
        &mut self,
        prefix: &'static [&'static str],
        router: &'static Router,
    ) -> &mut Self {
        self.handlers
            .push(Handler::formatted_router(prefix, router));
        self
    }

    pub fn add_unformatted_router(
        &mut self,
        prefix: &'static [&'static str],
        router: &'static Router,
    ) -> &mut Self {
        self.handlers
            .push(Handler::unformatted_router(prefix, router));
        self
    }
}

#[cfg(feature = "templates")]
mod templates {
    use std::collections::HashMap;
    use std::fs::metadata;
    use std::path::PathBuf;
    use std::sync::RwLock;
    use std::time::SystemTime;

    use anyhow::{bail, format_err, Error};
    use handlebars::Handlebars;
    use serde::Serialize;

    #[derive(Default)]
    pub struct Templates {
        templates: RwLock<Handlebars<'static>>,
        template_files: RwLock<HashMap<String, (SystemTime, PathBuf)>>,
    }

    impl Templates {
        pub fn register<P>(&self, name: &str, path: P) -> Result<(), Error>
        where
            P: Into<PathBuf>,
        {
            if self.template_files.read().unwrap().contains_key(name) {
                bail!("template already registered");
            }

            let path: PathBuf = path.into();
            let metadata = metadata(&path)?;
            let mtime = metadata.modified()?;

            self.templates
                .write()
                .unwrap()
                .register_template_file(name, &path)?;
            self.template_files
                .write()
                .unwrap()
                .insert(name.to_string(), (mtime, path));

            Ok(())
        }

        pub fn render<T>(&self, name: &str, data: &T) -> Result<String, Error>
        where
            T: Serialize,
        {
            let path;
            let mtime;
            {
                let template_files = self.template_files.read().unwrap();
                let (old_mtime, old_path) = template_files
                    .get(name)
                    .ok_or_else(|| format_err!("template not found"))?;

                mtime = metadata(old_path)?.modified()?;
                if mtime <= *old_mtime {
                    return self
                        .templates
                        .read()
                        .unwrap()
                        .render(name, data)
                        .map_err(|err| format_err!("{}", err));
                }
                path = old_path.to_path_buf();
            }

            {
                let mut template_files = self.template_files.write().unwrap();
                let mut templates = self.templates.write().unwrap();

                templates.register_template_file(name, &path)?;
                template_files.insert(name.to_string(), (mtime, path));

                templates
                    .render(name, data)
                    .map_err(|err| format_err!("{}", err))
            }
        }
    }
}

pub struct DummyAdapter;

impl ServerAdapter for DummyAdapter {
    fn get_index(
        &self,
        _rest_env: RestEnvironment,
        _parts: Parts,
    ) -> Pin<Box<dyn Future<Output = Response<Body>> + Send>> {
        Box::pin(async move {
            Response::builder()
                .status(400)
                .body("no index defined".into())
                .unwrap()
        })
    }

    fn check_auth<'a>(
        &'a self,
        _headers: &'a http::HeaderMap,
        _method: &'a http::Method,
    ) -> crate::ServerAdapterCheckAuth<'a> {
        Box::pin(async move { Err(crate::AuthError::NoData) })
    }
}
