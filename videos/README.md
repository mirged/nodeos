# videos/

Drop `.BAP` files here and `make` copies them to `/VIDEOS` on the NodeOS disk,
next to the homebrew demo it generates. Everything in this directory except
this file is ignored by git: videos stay on your machine.

Convert any video with:

```sh
user/video/tools/encode.sh some-video.mp4 videos/CLIP.BAP
user/video/tools/encode.sh --dither --start 1:00 --length 30 film.mkv videos/FILM.BAP
```

Keep names to FAT's 8.3 form (`CLIP.BAP`). `encode.sh --help` lists the options.
At the NodeOS shell, `video` opens the list; `video /VIDEOS/CLIP.BAP` plays one.
