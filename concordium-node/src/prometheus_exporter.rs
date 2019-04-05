use failure::Fallible;
use iron::{headers::ContentType, prelude::*, status};
use prometheus::{self, Encoder, IntCounter, IntGauge, Opts, Registry, TextEncoder};
use router::Router;
use std::{fmt, sync::Arc, thread, time};

#[derive(Clone, Debug, PartialEq, Copy)]
pub enum PrometheusMode {
    BootstrapperMode,
    NodeMode,
    IpDiscoveryMode,
}

impl fmt::Display for PrometheusMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            PrometheusMode::BootstrapperMode => write!(f, "bootstrapper"),
            PrometheusMode::NodeMode => write!(f, "node"),
            PrometheusMode::IpDiscoveryMode => write!(f, "ipdiscovery"),
        }
    }
}

#[derive(Clone)]
pub struct PrometheusServer {
    mode: PrometheusMode,
    registry: Registry,
    pkts_received_counter: IntCounter,
    pkts_sent_counter: IntCounter,
    peers_gauge: IntGauge,
    connections_received: IntCounter,
    unique_ips_seen: IntCounter,
    invalid_packets_received: IntCounter,
    unknown_packets_received: IntCounter,
    invalid_network_packets_received: IntCounter,
    queue_size: IntGauge,
    queue_resent: IntCounter,
}

impl PrometheusServer {
    pub fn new(mode: PrometheusMode) -> Self {
        let registry = Registry::new();
        let pg_opts = Opts::new("peer_number", "current peers connected");
        let pg = IntGauge::with_opts(pg_opts).unwrap();
        if mode == PrometheusMode::NodeMode || mode == PrometheusMode::BootstrapperMode {
            registry.register(Box::new(pg.clone())).unwrap();
        }

        let qs_opts = Opts::new("queue_size", "current queue size");
        let qs = IntGauge::with_opts(qs_opts).unwrap();
        if mode == PrometheusMode::NodeMode || mode == PrometheusMode::BootstrapperMode {
            registry.register(Box::new(qs.clone())).unwrap();
        }

        let cr_opts = Opts::new("conn_received", "connections received");
        let cr = IntCounter::with_opts(cr_opts).unwrap();
        registry.register(Box::new(cr.clone())).unwrap();

        let uis_opts = Opts::new("unique_ips_seen", "unique IPs seen");
        let uis = IntCounter::with_opts(uis_opts).unwrap();
        if mode == PrometheusMode::IpDiscoveryMode {
            registry.register(Box::new(uis.clone())).unwrap();
        }

        let prc_opts = Opts::new("packets_received", "packets received");
        let prc = IntCounter::with_opts(prc_opts).unwrap();
        registry.register(Box::new(prc.clone())).unwrap();

        let psc_opts = Opts::new("packets_sent", "packets sent");
        let psc = IntCounter::with_opts(psc_opts).unwrap();
        registry.register(Box::new(psc.clone())).unwrap();

        let ipr_opts = Opts::new("invalid_packets_received", "invalid packets received");
        let ipr = IntCounter::with_opts(ipr_opts).unwrap();
        if mode == PrometheusMode::NodeMode || mode == PrometheusMode::BootstrapperMode {
            registry.register(Box::new(ipr.clone())).unwrap();
        }

        let upr_opts = Opts::new("unknown_packets_received", "unknown packets received");
        let upr = IntCounter::with_opts(upr_opts).unwrap();
        if mode == PrometheusMode::NodeMode || mode == PrometheusMode::BootstrapperMode {
            registry.register(Box::new(upr.clone())).unwrap();
        }

        let inpr_opts = Opts::new(
            "invalid_network_packets_received",
            "invalid network packets received",
        );
        let inpr = IntCounter::with_opts(inpr_opts).unwrap();
        if mode == PrometheusMode::NodeMode || mode == PrometheusMode::BootstrapperMode {
            registry.register(Box::new(inpr.clone())).unwrap();
        }

        let qrs_opts = Opts::new("queue_resent", "items in queue that needed to be resent");
        let qrs = IntCounter::with_opts(qrs_opts).unwrap();
        if mode == PrometheusMode::NodeMode || mode == PrometheusMode::BootstrapperMode {
            registry.register(Box::new(qrs.clone())).unwrap();
        }

        PrometheusServer {
            mode,
            registry: registry.clone(),
            pkts_received_counter: prc.clone(),
            pkts_sent_counter: psc.clone(),
            peers_gauge: pg.clone(),
            connections_received: cr.clone(),
            unique_ips_seen: uis.clone(),
            invalid_packets_received: ipr.clone(),
            unknown_packets_received: upr.clone(),
            invalid_network_packets_received: inpr.clone(),
            queue_size: qs.clone(),
            queue_resent: qrs.clone(),
        }
    }

    pub fn peers_inc(&mut self) -> Fallible<()> {
        self.peers_gauge.inc();
        Ok(())
    }

    pub fn unique_ips_inc(&mut self) -> Fallible<()> {
        self.unique_ips_seen.inc();
        Ok(())
    }

    pub fn peers_dec(&mut self) -> Fallible<()> {
        self.peers_gauge.dec();
        Ok(())
    }

    pub fn peers_dec_by(&mut self, value: i64) -> Fallible<()> {
        self.peers_gauge.sub(value);
        Ok(())
    }

    pub fn pkt_received_inc(&mut self) -> Fallible<()> {
        self.pkts_received_counter.inc();
        Ok(())
    }

    pub fn pkt_received_inc_by(&mut self, to_add: i64) -> Fallible<()> {
        self.pkts_received_counter.inc_by(to_add);
        Ok(())
    }

    pub fn pkt_sent_inc(&mut self) -> Fallible<()> {
        self.pkts_sent_counter.inc();
        Ok(())
    }

    pub fn pkt_sent_inc_by(&mut self, to_add: i64) -> Fallible<()> {
        self.pkts_sent_counter.inc_by(to_add);
        Ok(())
    }

    pub fn conn_received_inc(&mut self) -> Fallible<()> {
        &self.connections_received.inc();
        Ok(())
    }

    pub fn invalid_pkts_received_inc(&mut self) -> Fallible<()> {
        self.invalid_packets_received.inc();
        Ok(())
    }

    pub fn invalid_network_pkts_received_inc(&mut self) -> Fallible<()> {
        self.invalid_network_packets_received.inc();
        Ok(())
    }

    pub fn unknown_pkts_received_inc(&mut self) -> Fallible<()> {
        self.unknown_packets_received.inc();
        Ok(())
    }

    pub fn queue_size_inc(&mut self) -> Fallible<()> {
        self.queue_size.inc();
        Ok(())
    }

    pub fn queue_size_dec(&mut self) -> Fallible<()> {
        self.queue_size.dec();
        Ok(())
    }

    pub fn queue_size_inc_by(&mut self, to_add: i64) -> Fallible<()> {
        self.queue_size.add(to_add);
        Ok(())
    }

    pub fn queue_resent_inc_by(&mut self, to_add: i64) -> Fallible<()> {
        self.queue_resent.inc_by(to_add);
        Ok(())
    }

    pub fn queue_size(&self) -> Fallible<(i64)> { Ok(self.queue_size.get()) }

    fn index(&self) -> IronResult<Response> {
        let mut resp = Response::with((
            status::Ok,
            format!(
                "<html><body><h1>Prometheus for {} v{}</h1>Operational!</p></body></html>",
                super::APPNAME,
                super::VERSION
            ),
        ));
        resp.headers.set(ContentType::html());
        Ok(resp)
    }

    fn metrics(&self) -> IronResult<Response> {
        let encoder = TextEncoder::new();
        let metric_familys = self.registry.gather();
        let mut buffer = vec![];
        encoder.encode(&metric_familys, &mut buffer).unwrap();
        let mut resp = Response::with((status::Ok, String::from_utf8(buffer).unwrap()));
        resp.headers.set(ContentType::plaintext());
        Ok(resp)
    }

    pub fn start_server(&mut self, listen_ip: &String, port: u16) -> Fallible<()> {
        let mut router = Router::new();
        let _self_clone = Arc::new(self.clone());
        let _self_clone_2 = _self_clone.clone();
        router.get(
            "/",
            move |_: &mut Request<'_, '_>| _self_clone.clone().index(),
            "index",
        );
        router.get(
            "/metrics",
            move |_: &mut Request<'_, '_>| _self_clone_2.clone().metrics(),
            "metrics",
        );
        let _listen = listen_ip.clone();
        let _th = thread::spawn(move || {
            Iron::new(router)
                .http(format!("{}:{}", _listen, port))
                .unwrap();
        });
        Ok(())
    }

    pub fn start_push_to_gateway(
        &self,
        prometheus_push_gateway: String,
        prometheus_push_interval: u64,
        prometheus_job_name: String,
        prometheus_instance_name: String,
        prometheus_push_username: Option<String>,
        prometheus_push_password: Option<String>,
    ) -> Fallible<()> {
        let _registry = self.registry.clone();
        let _mode = self.mode.clone();
        let _th = thread::spawn(move || loop {
            let username_pass =
                if prometheus_push_username.is_some() && prometheus_push_password.is_some() {
                    Some(prometheus::BasicAuthentication {
                        username: prometheus_push_username.clone().unwrap().to_owned(),
                        password: prometheus_push_password.clone().unwrap().to_owned(),
                    })
                } else {
                    None
                };
            debug!("Pushing data to push gateway");
            thread::sleep(time::Duration::from_secs(prometheus_push_interval));
            let metrics_families = _registry.gather();
            prometheus::push_metrics(&prometheus_job_name, labels!{"instance".to_owned() => prometheus_instance_name.clone(), "mode".to_owned() => _mode.to_string(),}, &prometheus_push_gateway, metrics_families, username_pass).map_err(|e| error!("{}", e)).ok();
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::prometheus_exporter::*;

    #[test]
    pub fn test_node_mode() { let _prom_inst = PrometheusServer::new(PrometheusMode::NodeMode); }

    #[test]
    pub fn test_disco_mode() {
        let _prom_inst = PrometheusServer::new(PrometheusMode::IpDiscoveryMode);
    }

    #[test]
    pub fn test_bootstrapper_mode() {
        let _prom_inst = PrometheusServer::new(PrometheusMode::BootstrapperMode);
    }
}
