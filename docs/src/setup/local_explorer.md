# Otterscan (Local Explorer)

Server supports `ots_` namespace and hence can be used in combination
with [Otterscan](https://github.com/otterscan/otterscan)
block explorer. To run a local instance as a Docker container (bound to `http://localhost:5100`):

```
docker run --rm -p 5100:80 --name otterscan -d --env ERIGON_URL="http://127.0.0.1:3050" otterscan/otterscan
```

See Otterscan's [docs](https://docs.otterscan.io/intro/) for other running options.
