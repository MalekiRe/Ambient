use std::{
    collections::HashMap, net::SocketAddr, path::{Path, PathBuf}, sync::Arc, time::SystemTime
};

use anyhow::Context;
use axum::{
    http::{Method, StatusCode}, response::IntoResponse, routing::{get, get_service}, Router
};
use elements_core::{app_start_time, asset_cache, dtime, no_sync, remove_at_time, time};
use elements_ecs::{components, EntityData, SystemGroup, World, WorldStreamCompEvent};
use elements_network::{
    bi_stream_handlers, client::GameRpcArgs, datagram_handlers, server::{ForkingEvent, GameServer, ShutdownEvent}
};
use elements_object::ObjectFromUrl;
use elements_rpc::RpcRegistry;
use elements_std::{
    asset_cache::{AssetCache, AsyncAssetKeyExt, SyncAssetKeyExt}, asset_url::{AbsAssetUrl, ServerBaseUrlKey}
};
use tower_http::{cors::CorsLayer, services::ServeDir};

use crate::{scripting, Cli, Commands};

components!("app", {
    project_path: PathBuf,
});

fn server_systems() -> SystemGroup {
    SystemGroup::new(
        "server",
        vec![
            Box::new(elements_core::async_ecs::async_ecs_systems()),
            Box::new(elements_core::transform::TransformSystem::new()),
            elements_core::remove_at_time_system(),
            Box::new(scripting::server::systems()),
        ],
    )
}
fn on_forking_systems() -> SystemGroup<ForkingEvent> {
    SystemGroup::new(
        "on_forking_systems",
        vec![Box::new(elements_physics::on_forking_systems()), Box::new(scripting::server::on_forking_systems())],
    )
}
fn on_shutdown_systems() -> SystemGroup<ShutdownEvent> {
    SystemGroup::new(
        "on_shutdown_systems",
        vec![Box::new(elements_physics::on_shutdown_systems()), Box::new(scripting::server::on_shutdown_systems())],
    )
}
fn is_sync_component(component: &dyn elements_ecs::IComponent, _event: WorldStreamCompEvent) -> bool {
    let res = component.is_extended() && component.clone_boxed() != remove_at_time();

    res
}

pub fn create_rpc_registry() -> RpcRegistry<GameRpcArgs> {
    let mut reg = RpcRegistry::new();
    elements_network::rpc::register_rpcs(&mut reg);
    reg
}

fn create_server_resources(assets: AssetCache, project_path: PathBuf) -> EntityData {
    let mut server_resources = EntityData::new().set(asset_cache(), assets).set(no_sync(), ());

    server_resources.append_self(elements_core::async_ecs::async_ecs_resources());
    server_resources.set_self(elements_core::runtime(), tokio::runtime::Handle::current());
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();
    server_resources.set_self(time(), now);
    server_resources.set_self(app_start_time(), now);
    server_resources.set_self(dtime(), 1. / 60.);
    server_resources.set_self(self::project_path(), project_path);

    let mut handlers = HashMap::new();
    elements_network::register_rpc_bi_stream_handler(&mut handlers, create_rpc_registry());
    server_resources.set_self(bi_stream_handlers(), handlers);

    let handlers = HashMap::new();
    server_resources.set_self(datagram_handlers(), handlers);

    server_resources
}

pub const HTTP_INTERFACE_PORT: u16 = 8999;
pub const QUIC_INTERFACE_PORT: u16 = 9000;

fn start_http_interface(runtime: &tokio::runtime::Runtime, project_path: &Path) {
    let router = Router::new()
        .route("/ping", get(|| async move { "ok" }))
        .nest("/assets", get_service(ServeDir::new(project_path.join("target"))).handle_error(handle_error))
        .layer(CorsLayer::new().allow_origin(tower_http::cors::Any).allow_methods(vec![Method::GET]).allow_headers(tower_http::cors::Any));

    runtime.spawn(async move {
        let addr = SocketAddr::from(([0, 0, 0, 0], HTTP_INTERFACE_PORT));
        axum::Server::bind(&addr).serve(router.into_make_service()).await.unwrap();
    });
}

async fn handle_error(_err: std::io::Error) -> impl IntoResponse {
    (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong...")
}

pub(crate) fn start_server(runtime: &tokio::runtime::Runtime, assets: AssetCache, cli: Cli, project_path: PathBuf) -> u16 {
    log::info!("Creating server");
    let server = runtime.block_on(async move {
        GameServer::new_with_port_in_range(QUIC_INTERFACE_PORT..(QUIC_INTERFACE_PORT + 10))
            .await
            .context("failed to create game server with port in range")
            .unwrap()
    });
    let port = server.port;
    log::info!("Server created on port {port}");

    init_components();
    scripting::server::init_all_components();
    ServerBaseUrlKey.insert(
        &assets,
        AbsAssetUrl::parse(format!("http://{}:{HTTP_INTERFACE_PORT}/assets/", cli.public_host.unwrap_or("localhost".to_string()))).unwrap(),
    );

    start_http_interface(runtime, &project_path);

    runtime.spawn(async move {
        let mut server_world = World::new_with_config("server", 1, true);
        server_world.init_shape_change_tracking();

        server_world.add_components(server_world.resource_entity(), create_server_resources(assets.clone(), project_path.clone())).unwrap();

        scripting::server::initialize(&mut server_world).await.unwrap();

        if let Commands::View { asset_path, .. } = cli.command.clone() {
            let asset_path = AbsAssetUrl::from_file_path(project_path.join("target").join(asset_path).join("objects/main.json"));
            log::info!("Spawning asset from {:?}", asset_path);
            let obj = ObjectFromUrl(asset_path).get(&assets).await.unwrap();
            obj.spawn_into_world(&mut server_world, None);
        }
        log::info!("Starting server");
        server
            .run(
                server_world,
                Arc::new(|_world| server_systems()),
                Arc::new(|| on_forking_systems()),
                Arc::new(|| on_shutdown_systems()),
                Arc::new(is_sync_component),
            )
            .await;
    });
    port
}
