# prometheus-http-client

Makes requests to the prometheus query API. With `plot` feature, also provides
a way to plot resposnes from prometheus.

*Note*: There are many prometheus query clients for rust -- I only made another because
I thought I only needed one or two routes so I would just use `reqwest` directly.

Over time it has grown and now I feel a bit silly.

This should probably be replaced
with [`prometheus-http-query`](https://docs.rs/prometheus-http-query/latest/prometheus_http_query/)
or one of the other competitors, except for the plotting functionality.
