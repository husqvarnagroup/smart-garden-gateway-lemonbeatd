# lemonbeatd-tests

These tests use `pytest` to simulate communication between devices and the `lemonbeatd` service.
Due to network isolation they need to run in a container.

## Install Dependencies

For Ubuntu:

```bash
apt install docker-ce docker-compose
```


## Run the tests


The tests can be run in Docker.

From the repository root directory run:
```
docker compose -f tests/docker/compose.yaml run component-tests --build --remove-orphans
```

