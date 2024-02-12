use std::{collections::HashMap, sync::Arc};

use opentelemetry::{
    global,
    metrics::{CallbackRegistration, ObservableGauge},
    KeyValue,
};
use slog::{error, info, Logger};
use tokio::sync::Mutex;

const NETWORK: &str = "network";
const AXUM_APP: &str = "axum-app";
const LOAD: &str = "load";
const SYNC: &str = "sync";

type StatusCallbacks = Arc<Mutex<HashMap<String, Vec<Box<dyn CallbackRegistration>>>>>;
type ValueCallbacks = Arc<Mutex<HashMap<String, Vec<NamedCallbackWithValue<i64>>>>>;

#[derive(Clone)]
pub struct MSDMetrics {
    pub running_definition_metrics: RunningDefinitionsMetrics,
}

impl Default for MSDMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl MSDMetrics {
    pub fn new() -> Self {
        Self {
            running_definition_metrics: RunningDefinitionsMetrics::new(),
        }
    }
}

#[derive(Clone)]
pub struct RunningDefinitionsMetrics {
    pub load_new_targets_error: ObservableGauge<i64>,
    pub definitions_load_successful: ObservableGauge<i64>,

    pub sync_registry_error: ObservableGauge<i64>,
    pub definitions_sync_successful: ObservableGauge<i64>,

    definition_status_callbacks: StatusCallbacks,
    definition_value_callbacks: ValueCallbacks,
}

impl RunningDefinitionsMetrics {
    pub fn new() -> Self {
        let meter = global::meter(AXUM_APP);
        let load_new_targets_error = meter
            .i64_observable_gauge("msd.definitions.load.errors")
            .with_description("Total number of errors while loading new targets per definition")
            .init();

        let sync_registry_error = meter
            .i64_observable_gauge("msd.definitions.sync.errors")
            .with_description("Total number of errors while syncing the registry per definition")
            .init();

        let definitions_load_successful = meter
            .i64_observable_gauge("msd.definitions.load.successful")
            .with_description("Status of last load of the registry per definition")
            .init();

        let definitions_sync_successful = meter
            .i64_observable_gauge("msd.definitions.sync.successful")
            .with_description("Status of last sync of the registry with NNS of definition")
            .init();

        Self {
            load_new_targets_error,
            definitions_load_successful,
            sync_registry_error,
            definitions_sync_successful,
            definition_status_callbacks: Arc::new(Mutex::new(HashMap::new())),
            definition_value_callbacks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn inc_load_errors(&self, network: String, logger: Logger) {
        Self::inc_counter(
            network,
            logger,
            &self.definition_value_callbacks,
            &self.load_new_targets_error,
            LOAD.to_string(),
        )
        .await
    }

    pub async fn inc_sync_errors(&self, network: String, logger: Logger) {
        Self::inc_counter(
            network,
            logger,
            &self.definition_value_callbacks,
            &self.sync_registry_error,
            SYNC.to_string(),
        )
        .await
    }

    pub async fn set_successful_sync(&mut self, network: String, logger: Logger) {
        Self::set_status(
            network,
            logger,
            1,
            &self.definitions_sync_successful,
            &self.definition_status_callbacks,
        )
        .await
    }

    pub async fn set_failed_sync(&mut self, network: String, logger: Logger) {
        Self::set_status(
            network,
            logger,
            0,
            &self.definitions_sync_successful,
            &self.definition_status_callbacks,
        )
        .await
    }

    pub async fn set_successful_load(&mut self, network: String, logger: Logger) {
        Self::set_status(
            network,
            logger,
            1,
            &self.definitions_load_successful,
            &self.definition_status_callbacks,
        )
        .await
    }

    pub async fn set_failed_load(&mut self, network: String, logger: Logger) {
        Self::set_status(
            network,
            logger,
            0,
            &self.definitions_load_successful,
            &self.definition_status_callbacks,
        )
        .await
    }

    async fn set_status(
        network: String,
        logger: Logger,
        status: i64,
        gague: &ObservableGauge<i64>,
        callbacks: &StatusCallbacks,
    ) {
        let meter = global::meter(AXUM_APP);
        let network_clone = network.clone();
        let local_clone = gague.clone();

        match meter.register_callback(&[local_clone.as_any()], move |observer| {
            observer.observe_i64(&local_clone, status, &[KeyValue::new(NETWORK, network.clone())])
        }) {
            Ok(callback) => {
                info!(logger, "Registering callback for '{}'", &network_clone);
                let mut locked = callbacks.lock().await;

                if let Some(definition) = locked.get_mut(&network_clone) {
                    definition.push(callback)
                } else {
                    locked.insert(network_clone, vec![callback]);
                }
            }
            Err(e) => error!(
                logger,
                "Couldn't register callback for network '{}': {:?}", network_clone, e
            ),
        }
    }

    pub async fn unregister_callback(&self, network: String, logger: Logger) {
        self.unregister_unnamed_callback(network.clone(), logger.clone()).await;
        self.unregister_named_callback(network, logger).await
    }

    async fn unregister_named_callback(&self, network: String, logger: Logger) {
        let mut locked = self.definition_value_callbacks.lock().await;

        if let Some(callbacks) = locked.remove(&network) {
            for mut nc in callbacks {
                if let Err(e) = nc.callback.unregister() {
                    error!(
                        logger,
                        "Couldn't unregister callback for network '{}': {:?}", network, e
                    )
                }
            }
        }
    }

    async fn unregister_unnamed_callback(&self, network: String, logger: Logger) {
        let mut locked = self.definition_status_callbacks.lock().await;

        if let Some(callbacks) = locked.remove(&network) {
            for mut callback in callbacks {
                if let Err(e) = callback.unregister() {
                    error!(
                        logger,
                        "Couldn't unregister callback for network '{}': {:?}", network, e
                    )
                }
            }
        } else {
            error!(
                logger,
                "Couldn't unregister callbacks for network '{}': key not found", &network
            )
        }
    }

    async fn inc_counter(
        network: String,
        logger: Logger,
        callbacks: &ValueCallbacks,
        counter: &ObservableGauge<i64>,
        metric_name: String,
    ) {
        let mut locked = callbacks.lock().await;
        let network_clone = network.clone();
        let meter = global::meter(AXUM_APP);
        let local_clone = counter.clone();

        match locked.get_mut(&network) {
            Some(callbacks) => match callbacks.iter_mut().find(|nc| nc.name == metric_name) {
                Some(nc) => {
                    info!(logger, "Updating the named callback for network '{}'", network.clone());
                    if let Err(e) = nc.callback.unregister() {
                        error!(logger, "Couldn't unregister metric for network '{}': {:?}", network, e);
                        return;
                    }

                    nc.value += 1;
                    let cloned = nc.value;

                    match meter.register_callback(&[local_clone.as_any()], move |observer| {
                        observer.observe_i64(&local_clone, cloned, &[KeyValue::new(NETWORK, network.clone())])
                    }) {
                        Ok(callback) => nc.callback = callback,
                        Err(e) => {
                            error!(
                                logger,
                                "Couldn't register counter for network '{}': {:?}", network_clone, e
                            )
                        }
                    }
                }
                None => {
                    match meter.register_callback(&[local_clone.as_any()], move |observer| {
                        observer.observe_i64(&local_clone, 1, &[KeyValue::new(NETWORK, network.clone())])
                    }) {
                        Ok(callback) => {
                            let named = NamedCallbackWithValue {
                                value: 1_i64,
                                callback,
                                name: metric_name,
                            };

                            callbacks.push(named)
                        }
                        Err(e) => {
                            error!(
                                logger,
                                "Couldn't register counter for network '{}': {:?}", network_clone, e
                            )
                        }
                    }
                }
            },
            None => {
                match meter.register_callback(&[local_clone.as_any()], move |observer| {
                    observer.observe_i64(&local_clone, 1, &[KeyValue::new(NETWORK, network.clone())])
                }) {
                    Ok(callback) => {
                        info!(logger, "Registering new counter for '{}'", network_clone);
                        let named = NamedCallbackWithValue {
                            value: 1_i64,
                            callback,
                            name: metric_name,
                        };

                        locked.insert(network_clone, vec![named]);
                    }
                    Err(e) => {
                        error!(
                            logger,
                            "Couldn't register counter for network '{}': {:?}", network_clone, e
                        )
                    }
                }
            }
        }
    }
}

struct NamedCallbackWithValue<T> {
    callback: Box<dyn CallbackRegistration>,
    value: T,
    name: String,
}
