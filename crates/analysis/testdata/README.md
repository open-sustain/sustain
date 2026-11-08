Decoder fixtures: one second of a 440 Hz mono sine at 44.1 kHz in an M4A
container, one file per codec an M4A carries. Generated with:

```
ffmpeg -f lavfi -i "sine=frequency=440:duration=1:sample_rate=44100" -ac 1 \
  -c:a aac -b:a 32k -map_metadata -1 -fflags +bitexact -flags:a +bitexact sine-aac.m4a
ffmpeg -f lavfi -i "sine=frequency=440:duration=1:sample_rate=44100" -ac 1 \
  -c:a alac -map_metadata -1 -fflags +bitexact -flags:a +bitexact sine-alac.m4a
```
