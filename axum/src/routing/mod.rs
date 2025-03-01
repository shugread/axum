//! Routing between [`Service`]s and handlers.

use self::{future::RouteFuture, not_found::NotFound, path_router::PathRouter};
#[cfg(feature = "tokio")]
use crate::extract::connect_info::IntoMakeServiceWithConnectInfo;
use crate::{
    body::{Body, HttpBody},
    boxed::BoxedIntoRoute,
    handler::Handler,
    util::try_downcast,
};
use axum_core::{
    extract::Request,
    response::{IntoResponse, Response},
};
use std::{
    convert::Infallible,
    fmt,
    marker::PhantomData,
    sync::Arc,
    task::{Context, Poll},
};
use tower_layer::Layer;
use tower_service::Service;

pub mod future;
pub mod method_routing;

mod into_make_service;
mod method_filter;
mod not_found;
pub(crate) mod path_router;
mod route;
mod strip_prefix;
pub(crate) mod url_params;

#[cfg(test)]
mod tests;

pub use self::{into_make_service::IntoMakeService, method_filter::MethodFilter, route::Route};

pub use self::method_routing::{
    any, any_service, delete, delete_service, get, get_service, head, head_service, on, on_service,
    options, options_service, patch, patch_service, post, post_service, put, put_service, trace,
    trace_service, MethodRouter,
};

macro_rules! panic_on_err {
    ($expr:expr) => {
        match $expr {
            Ok(x) => x,
            Err(err) => panic!("{err}"),
        }
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct RouteId(u32);

/// The router type for composing handlers and services.
/// 用于组成Handler和Service的路由器类型.
#[must_use]
pub struct Router<S = ()> {
    inner: Arc<RouterInner<S>>,
}

impl<S> Clone for Router<S> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct RouterInner<S> {
    path_router: PathRouter<S, false>,
    fallback_router: PathRouter<S, true>,
    default_fallback: bool,
    catch_all_fallback: Fallback<S>,
}

impl<S> Default for Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<S> fmt::Debug for Router<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Router")
            .field("path_router", &self.inner.path_router)
            .field("fallback_router", &self.inner.fallback_router)
            .field("default_fallback", &self.inner.default_fallback)
            .field("catch_all_fallback", &self.inner.catch_all_fallback)
            .finish()
    }
}

pub(crate) const NEST_TAIL_PARAM: &str = "__private__axum_nest_tail_param";
pub(crate) const NEST_TAIL_PARAM_CAPTURE: &str = "/*__private__axum_nest_tail_param";
pub(crate) const FALLBACK_PARAM: &str = "__private__axum_fallback";
pub(crate) const FALLBACK_PARAM_PATH: &str = "/*__private__axum_fallback";

impl<S> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    /// Create a new `Router`.
    ///
    /// Unless you add additional routes this will respond with `404 Not Found` to
    /// all requests.
    /// 创建路由
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RouterInner {
                path_router: Default::default(),
                fallback_router: PathRouter::new_fallback(),
                default_fallback: true,
                catch_all_fallback: Fallback::Default(Route::new(NotFound)),
            }),
        }
    }

    // 转换Router
    fn map_inner<F, S2>(self, f: F) -> Router<S2>
    where
        F: FnOnce(RouterInner<S>) -> RouterInner<S2>,
    {
        Router {
            inner: Arc::new(f(self.into_inner())),
        }
    }

    // 修改inner
    fn tap_inner_mut<F>(self, f: F) -> Self
    where
        F: FnOnce(&mut RouterInner<S>),
    {
        let mut inner = self.into_inner();
        f(&mut inner);
        Router {
            inner: Arc::new(inner),
        }
    }

    // 返回inner
    fn into_inner(self) -> RouterInner<S> {
        match Arc::try_unwrap(self.inner) {
            Ok(inner) => inner,
            Err(arc) => RouterInner {
                path_router: arc.path_router.clone(),
                fallback_router: arc.fallback_router.clone(),
                default_fallback: arc.default_fallback,
                catch_all_fallback: arc.catch_all_fallback.clone(),
            },
        }
    }

    #[doc = include_str!("../docs/routing/route.md")]
    #[track_caller]
    // 插入路由
    pub fn route(self, path: &str, method_router: MethodRouter<S>) -> Self {
        self.tap_inner_mut(|this| {
            panic_on_err!(this.path_router.route(path, method_router));
        })
    }

    #[doc = include_str!("../docs/routing/route_service.md")]
    pub fn route_service<T>(self, path: &str, service: T) -> Self
    where
        T: Service<Request, Error = Infallible> + Clone + Send + 'static,
        T::Response: IntoResponse,
        T::Future: Send + 'static,
    {
        let service = match try_downcast::<Router<S>, _>(service) {
            Ok(_) => {
                panic!(
                    "Invalid route: `Router::route_service` cannot be used with `Router`s. \
                     Use `Router::nest` instead"
                );
            }
            Err(service) => service,
        };

        // 添加Service
        self.tap_inner_mut(|this| {
            panic_on_err!(this.path_router.route_service(path, service));
        })
    }

    #[doc = include_str!("../docs/routing/nest.md")]
    #[doc(alias = "scope")] // Some web frameworks like actix-web use this term
    #[track_caller]
    // 嵌套路由
    pub fn nest(self, path: &str, router: Router<S>) -> Self {
        let RouterInner {
            path_router,
            fallback_router,
            default_fallback,
            // we don't need to inherit the catch-all fallback. It is only used for CONNECT
            // requests with an empty path. If we were to inherit the catch-all fallback
            // it would end up matching `/{path}/*` which doesn't match empty paths.
            catch_all_fallback: _,
        } = router.into_inner();

        self.tap_inner_mut(|this| {
            panic_on_err!(this.path_router.nest(path, path_router));

            if !default_fallback {
                panic_on_err!(this.fallback_router.nest(path, fallback_router));
            }
        })
    }

    /// Like [`nest`](Self::nest), but accepts an arbitrary `Service`.
    /// 嵌套服务
    #[track_caller]
    pub fn nest_service<T>(self, path: &str, service: T) -> Self
    where
        T: Service<Request, Error = Infallible> + Clone + Send + 'static,
        T::Response: IntoResponse,
        T::Future: Send + 'static,
    {
        self.tap_inner_mut(|this| {
            panic_on_err!(this.path_router.nest_service(path, service));
        })
    }

    #[doc = include_str!("../docs/routing/merge.md")]
    #[track_caller]
    // 合并路由
    pub fn merge<R>(self, other: R) -> Self
    where
        R: Into<Router<S>>,
    {
        const PANIC_MSG: &str =
            "Failed to merge fallbacks. This is a bug in axum. Please file an issue";

        let other: Router<S> = other.into();
        let RouterInner {
            path_router,
            fallback_router: mut other_fallback,
            default_fallback,
            catch_all_fallback,
        } = other.into_inner();

        self.map_inner(|mut this| {
            panic_on_err!(this.path_router.merge(path_router));

            match (this.default_fallback, default_fallback) {
                // both have the default fallback
                // use the one from other
                // 两者都有fallback_router,other
                (true, true) => {
                    this.fallback_router.merge(other_fallback).expect(PANIC_MSG);
                }
                // this has default fallback, other has a custom fallback
                // this有default_fallback,other有自定义default_fallback
                (true, false) => {
                    this.fallback_router.merge(other_fallback).expect(PANIC_MSG);
                    this.default_fallback = false;
                }
                // this has a custom fallback, other has a default
                (false, true) => {
                    let fallback_router = std::mem::take(&mut this.fallback_router);
                    other_fallback.merge(fallback_router).expect(PANIC_MSG);
                    this.fallback_router = other_fallback;
                }
                // both have a custom fallback, not allowed
                // 不允许,都有自定义fallback
                (false, false) => {
                    panic!("Cannot merge two `Router`s that both have a fallback")
                }
            };

            this.catch_all_fallback = this
                .catch_all_fallback
                .merge(catch_all_fallback)
                .unwrap_or_else(|| panic!("Cannot merge two `Router`s that both have a fallback"));

            this
        })
    }

    #[doc = include_str!("../docs/routing/layer.md")]
    // 使用layer
    pub fn layer<L>(self, layer: L) -> Router<S>
    where
        L: Layer<Route> + Clone + Send + 'static,
        L::Service: Service<Request> + Clone + Send + 'static,
        <L::Service as Service<Request>>::Response: IntoResponse + 'static,
        <L::Service as Service<Request>>::Error: Into<Infallible> + 'static,
        <L::Service as Service<Request>>::Future: Send + 'static,
    {
        self.map_inner(|this| RouterInner {
            path_router: this.path_router.layer(layer.clone()),
            fallback_router: this.fallback_router.layer(layer.clone()),
            default_fallback: this.default_fallback,
            catch_all_fallback: this.catch_all_fallback.map(|route| route.layer(layer)),
        })
    }

    #[doc = include_str!("../docs/routing/route_layer.md")]
    #[track_caller]
    // 路由layer
    pub fn route_layer<L>(self, layer: L) -> Self
    where
        L: Layer<Route> + Clone + Send + 'static,
        L::Service: Service<Request> + Clone + Send + 'static,
        <L::Service as Service<Request>>::Response: IntoResponse + 'static,
        <L::Service as Service<Request>>::Error: Into<Infallible> + 'static,
        <L::Service as Service<Request>>::Future: Send + 'static,
    {
        self.map_inner(|this| RouterInner {
            path_router: this.path_router.route_layer(layer),
            fallback_router: this.fallback_router,
            default_fallback: this.default_fallback,
            catch_all_fallback: this.catch_all_fallback,
        })
    }

    /// True if the router currently has at least one route added.
    pub fn has_routes(&self) -> bool {
        self.inner.path_router.has_routes()
    }

    #[track_caller]
    #[doc = include_str!("../docs/routing/fallback.md")]
    // 自定义fallback
    pub fn fallback<H, T>(self, handler: H) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        self.tap_inner_mut(|this| {
            this.catch_all_fallback =
                Fallback::BoxedHandler(BoxedIntoRoute::from_handler(handler.clone()));
        })
        .fallback_endpoint(Endpoint::MethodRouter(any(handler)))
    }

    /// Add a fallback [`Service`] to the router.
    ///
    /// See [`Router::fallback`] for more details.
    /// 使用Serveice作为fallback
    pub fn fallback_service<T>(self, service: T) -> Self
    where
        T: Service<Request, Error = Infallible> + Clone + Send + 'static,
        T::Response: IntoResponse,
        T::Future: Send + 'static,
    {
        let route = Route::new(service);
        self.tap_inner_mut(|this| {
            this.catch_all_fallback = Fallback::Service(route.clone());
        })
        .fallback_endpoint(Endpoint::Route(route))
    }

    fn fallback_endpoint(self, endpoint: Endpoint<S>) -> Self {
        self.tap_inner_mut(|this| {
            this.fallback_router.set_fallback(endpoint);
            this.default_fallback = false;
        })
    }

    #[doc = include_str!("../docs/routing/with_state.md")]
    // 添加状态
    pub fn with_state<S2>(self, state: S) -> Router<S2> {
        self.map_inner(|this| RouterInner {
            path_router: this.path_router.with_state(state.clone()),
            fallback_router: this.fallback_router.with_state(state.clone()),
            default_fallback: this.default_fallback,
            catch_all_fallback: this.catch_all_fallback.with_state(state),
        })
    }

    // 调用
    pub(crate) fn call_with_state(&self, req: Request, state: S) -> RouteFuture<Infallible> {
        // 先使用path_router查找处理器
        let (req, state) = match self.inner.path_router.call_with_state(req, state) {
            Ok(future) => return future,
            Err((req, state)) => (req, state),
        };

        // 使用fallback
        let (req, state) = match self.inner.fallback_router.call_with_state(req, state) {
            Ok(future) => return future,
            Err((req, state)) => (req, state),
        };

        // 使用捕获所有的fallback
        self.inner
            .catch_all_fallback
            .clone()
            .call_with_state(req, state)
    }

    /// Convert the router into a borrowed [`Service`] with a fixed request body type, to aid type
    /// inference.
    ///
    /// In some cases when calling methods from [`tower::ServiceExt`] on a [`Router`] you might get
    /// type inference errors along the lines of
    ///
    /// ```not_rust
    /// let response = router.ready().await?.call(request).await?;
    ///                       ^^^^^ cannot infer type for type parameter `B`
    /// ```
    ///
    /// This happens because `Router` implements [`Service`] with `impl<B> Service<Request<B>> for Router<()>`.
    ///
    /// For example:
    ///
    /// ```compile_fail
    /// use axum::{
    ///     Router,
    ///     routing::get,
    ///     http::Request,
    ///     body::Body,
    /// };
    /// use tower::{Service, ServiceExt};
    ///
    /// # async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut router = Router::new().route("/", get(|| async {}));
    /// let request = Request::new(Body::empty());
    /// let response = router.ready().await?.call(request).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Calling `Router::as_service` fixes that:
    ///
    /// ```
    /// use axum::{
    ///     Router,
    ///     routing::get,
    ///     http::Request,
    ///     body::Body,
    /// };
    /// use tower::{Service, ServiceExt};
    ///
    /// # async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut router = Router::new().route("/", get(|| async {}));
    /// let request = Request::new(Body::empty());
    /// let response = router.as_service().ready().await?.call(request).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// This is mainly used when calling `Router` in tests. It shouldn't be necessary when running
    /// the `Router` normally via [`Router::into_make_service`].
    pub fn as_service<B>(&mut self) -> RouterAsService<'_, B, S> {
        RouterAsService {
            router: self,
            _marker: PhantomData,
        }
    }

    /// Convert the router into an owned [`Service`] with a fixed request body type, to aid type
    /// inference.
    ///
    /// This is the same as [`Router::as_service`] instead it returns an owned [`Service`]. See
    /// that method for more details.
    /// 转换成Serveice
    pub fn into_service<B>(self) -> RouterIntoService<B, S> {
        RouterIntoService {
            router: self,
            _marker: PhantomData,
        }
    }
}

impl Router {
    /// Convert this router into a [`MakeService`], that is a [`Service`] whose
    /// response is another service.
    ///
    /// ```
    /// use axum::{
    ///     routing::get,
    ///     Router,
    /// };
    ///
    /// let app = Router::new().route("/", get(|| async { "Hi!" }));
    ///
    /// # async {
    /// let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    /// axum::serve(listener, app).await.unwrap();
    /// # };
    /// ```
    ///
    /// [`MakeService`]: tower::make::MakeService
    pub fn into_make_service(self) -> IntoMakeService<Self> {
        // call `Router::with_state` such that everything is turned into `Route` eagerly
        // rather than doing that per request
        IntoMakeService::new(self.with_state(()))
    }

    #[doc = include_str!("../docs/routing/into_make_service_with_connect_info.md")]
    #[cfg(feature = "tokio")]
    pub fn into_make_service_with_connect_info<C>(self) -> IntoMakeServiceWithConnectInfo<Self, C> {
        // call `Router::with_state` such that everything is turned into `Route` eagerly
        // rather than doing that per request
        IntoMakeServiceWithConnectInfo::new(self.with_state(()))
    }
}

// for `axum::serve(listener, router)`
#[cfg(all(feature = "tokio", any(feature = "http1", feature = "http2")))]
const _: () = {
    use crate::serve::IncomingStream;

    impl Service<IncomingStream<'_>> for Router<()> {
        type Response = Self;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: IncomingStream<'_>) -> Self::Future {
            // call `Router::with_state` such that everything is turned into `Route` eagerly
            // rather than doing that per request
            std::future::ready(Ok(self.clone().with_state(())))
        }
    }
};

impl<B> Service<Request<B>> for Router<()>
where
    B: HttpBody<Data = bytes::Bytes> + Send + 'static,
    B::Error: Into<axum_core::BoxError>,
{
    type Response = Response;
    type Error = Infallible;
    type Future = RouteFuture<Infallible>;

    #[inline]
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&mut self, req: Request<B>) -> Self::Future {
        let req = req.map(Body::new);
        self.call_with_state(req, ())
    }
}

/// A [`Router`] converted into a borrowed [`Service`] with a fixed body type.
///
/// See [`Router::as_service`] for more details.
pub struct RouterAsService<'a, B, S = ()> {
    router: &'a mut Router<S>,
    _marker: PhantomData<B>,
}

impl<'a, B> Service<Request<B>> for RouterAsService<'a, B, ()>
where
    B: HttpBody<Data = bytes::Bytes> + Send + 'static,
    B::Error: Into<axum_core::BoxError>,
{
    type Response = Response;
    type Error = Infallible;
    type Future = RouteFuture<Infallible>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        <Router as Service<Request<B>>>::poll_ready(self.router, cx)
    }

    #[inline]
    fn call(&mut self, req: Request<B>) -> Self::Future {
        self.router.call(req)
    }
}

impl<'a, B, S> fmt::Debug for RouterAsService<'a, B, S>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RouterAsService")
            .field("router", &self.router)
            .finish()
    }
}

/// A [`Router`] converted into an owned [`Service`] with a fixed body type.
///
/// See [`Router::into_service`] for more details.
pub struct RouterIntoService<B, S = ()> {
    router: Router<S>,
    _marker: PhantomData<B>,
}

impl<B, S> Clone for RouterIntoService<B, S>
where
    Router<S>: Clone,
{
    fn clone(&self) -> Self {
        Self {
            router: self.router.clone(),
            _marker: PhantomData,
        }
    }
}

impl<B> Service<Request<B>> for RouterIntoService<B, ()>
where
    B: HttpBody<Data = bytes::Bytes> + Send + 'static,
    B::Error: Into<axum_core::BoxError>,
{
    type Response = Response;
    type Error = Infallible;
    type Future = RouteFuture<Infallible>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        <Router as Service<Request<B>>>::poll_ready(&mut self.router, cx)
    }

    #[inline]
    fn call(&mut self, req: Request<B>) -> Self::Future {
        self.router.call(req)
    }
}

impl<B, S> fmt::Debug for RouterIntoService<B, S>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RouterIntoService")
            .field("router", &self.router)
            .finish()
    }
}

// fallback结构
enum Fallback<S, E = Infallible> {
    Default(Route<E>),
    Service(Route<E>),
    BoxedHandler(BoxedIntoRoute<S, E>),
}

impl<S, E> Fallback<S, E>
where
    S: Clone,
{
    // 合并
    fn merge(self, other: Self) -> Option<Self> {
        match (self, other) {
            // 都是默认的,选择self的
            (Self::Default(_), pick @ Self::Default(_)) => Some(pick),
            // 选择设置了的
            (Self::Default(_), pick) | (pick, Self::Default(_)) => Some(pick),
            _ => None,
        }
    }

    // 转换
    fn map<F, E2>(self, f: F) -> Fallback<S, E2>
    where
        S: 'static,
        E: 'static,
        F: FnOnce(Route<E>) -> Route<E2> + Clone + Send + 'static,
        E2: 'static,
    {
        match self {
            Self::Default(route) => Fallback::Default(f(route)),
            Self::Service(route) => Fallback::Service(f(route)),
            Self::BoxedHandler(handler) => Fallback::BoxedHandler(handler.map(f)),
        }
    }

    // 将state保存到Route
    fn with_state<S2>(self, state: S) -> Fallback<S2, E> {
        match self {
            Fallback::Default(route) => Fallback::Default(route),
            Fallback::Service(route) => Fallback::Service(route),
            Fallback::BoxedHandler(handler) => Fallback::Service(handler.into_route(state)),
        }
    }

    // 调用
    fn call_with_state(&mut self, req: Request, state: S) -> RouteFuture<E> {
        match self {
            Fallback::Default(route) | Fallback::Service(route) => {
                RouteFuture::from_future(route.oneshot_inner(req))
            }
            Fallback::BoxedHandler(handler) => {
                let mut route = handler.clone().into_route(state);
                RouteFuture::from_future(route.oneshot_inner(req))
            }
        }
    }
}

impl<S, E> Clone for Fallback<S, E> {
    fn clone(&self) -> Self {
        match self {
            Self::Default(inner) => Self::Default(inner.clone()),
            Self::Service(inner) => Self::Service(inner.clone()),
            Self::BoxedHandler(inner) => Self::BoxedHandler(inner.clone()),
        }
    }
}

impl<S, E> fmt::Debug for Fallback<S, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default(inner) => f.debug_tuple("Default").field(inner).finish(),
            Self::Service(inner) => f.debug_tuple("Service").field(inner).finish(),
            Self::BoxedHandler(_) => f.debug_tuple("BoxedHandler").finish(),
        }
    }
}

// path的端点
#[allow(clippy::large_enum_variant)]
enum Endpoint<S> {
    MethodRouter(MethodRouter<S>),
    Route(Route),
}

impl<S> Endpoint<S>
where
    S: Clone + Send + Sync + 'static,
{
    fn layer<L>(self, layer: L) -> Endpoint<S>
    where
        L: Layer<Route> + Clone + Send + 'static,
        L::Service: Service<Request> + Clone + Send + 'static,
        <L::Service as Service<Request>>::Response: IntoResponse + 'static,
        <L::Service as Service<Request>>::Error: Into<Infallible> + 'static,
        <L::Service as Service<Request>>::Future: Send + 'static,
    {
        match self {
            Endpoint::MethodRouter(method_router) => {
                Endpoint::MethodRouter(method_router.layer(layer))
            }
            Endpoint::Route(route) => Endpoint::Route(route.layer(layer)),
        }
    }
}

impl<S> Clone for Endpoint<S> {
    fn clone(&self) -> Self {
        match self {
            Self::MethodRouter(inner) => Self::MethodRouter(inner.clone()),
            Self::Route(inner) => Self::Route(inner.clone()),
        }
    }
}

impl<S> fmt::Debug for Endpoint<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MethodRouter(method_router) => {
                f.debug_tuple("MethodRouter").field(method_router).finish()
            }
            Self::Route(route) => f.debug_tuple("Route").field(route).finish(),
        }
    }
}

#[test]
fn traits() {
    use crate::test_helpers::*;
    assert_send::<Router<()>>();
    assert_sync::<Router<()>>();
}
