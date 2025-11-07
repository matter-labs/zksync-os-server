# Docker

```
sudo docker build -t zksync_os_sequencer .
sudo docker run -d --name sequencer -p 3050:3050 -p 3124:3124 -p 3312:3312 -e batcher_maximum_in_flight_blocks=15  -v /mnt/localssd/db:/db   zksync_os_sequencer
```
