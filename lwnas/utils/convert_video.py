#!/usr/bin/env python3
import time
import subprocess
import argparse
import re
import difflib
from pathlib import Path

VERSION = "0.0.1"
VIDEO_EXTENSIONS = ['.avi', '.mp4', '.rmvb', '.mkv']
SUBTITLE_EXTENSIONS = {'.ass', '.srt', '.vtt', '.ssa'}

def main():
	parser = argparse.ArgumentParser(
		prog="convert_video",
		description="convert any format to mp4 by ffmpeg, part of LwNAS - Que's Software",
		epilog=VERSION
	)
	parser.add_argument("-V", "--version", action="version", version=VERSION)
	parser.add_argument("--dry-run", help="dry run", action="store_true")
	parser.add_argument("-s", help="detect and apply subtitles", action="store_true")
	parser.add_argument("input", help="input file")
	parser.add_argument("output", help="output file")
	args = parser.parse_args()

	convert_file(args)

def is_subtitle_file(ifn):
	return (ifn.is_file() and ifn.suffix.lower() in SUBTITLE_EXTENSIONS)

def fmt_human_time(seconds):
	MINUTE = 60
	HOUR = 60 * MINUTE
	DAY = 24 * HOUR

	d = int(seconds // DAY)
	seconds %= DAY

	h = int(seconds // HOUR)
	seconds %= HOUR

	m = int(seconds // MINUTE)
	seconds %= MINUTE

	s = ''
	if d:
		s += f"{d}d "
	if h:
		s += f"{h}h "
	if m:
		s += f"{m}m "
	s += f"{seconds:.2f}s"
	return s

def log(*objs, sep=' ', end='\n'):
	print(time.strftime("[%m/%d/%Y %H:%M:%S]"), *objs)

def escape_subtitles_path(p):
	p = str(p).replace("\\", "/")
	if re.match(r"^[A-Za-z]:", p):
		p = p[0] + "\\:" + p[2:]
	return f"'{p}'"

def convert_file(args):
	ifn = Path(args.input)
	ofn = Path(args.output)
	log(f"{ifn} -> {ofn} ...")

	start = time.perf_counter()

	command = [
        "ffmpeg",
        "-i", str(ifn),

		# 音频
        "-c:a", "aac",
        "-b:a", "192k",

		# 视频
		"-crf", "18",
        "-c:v", "libx264",

		# 字幕
		"-c:s", "mov_text",
		"-map", "0",

        "-movflags",
		"+faststart",

		"-y",
		"-f", "mp4",
		# -vf "subtitles=sub.srt"
        # ofn,
    ]

	if args.s:
		srt = probe_subtitle_file(ifn)
		if srt:
			command.append("-vf")
			command.append(f"subtitles={escape_subtitles_path(srt)}")
			log(f"subtitle \"{srt}\" added")

	command.append(str(ofn))

	if args.dry_run:
		cmd_str = ' '.join(map(lambda x: str(x), command))
		log(f"{cmd_str}")
	else:
		try:
			# 执行命令并捕获标准输出和标准错误
			result = subprocess.run(
				command,
				capture_output=True,
				# text=True,
				check=True
			)
		except subprocess.CalledProcessError as e:
			log(f"FFmpeg failed with exit code: {e.returncode}")
		except FileNotFoundError:
			log(f"FFmpeg not installed")
			exit(1)

	end = time.perf_counter()
	log(f"{ifn} -> {ofn} done [{fmt_human_time(end - start)}]")

def probe_subtitle_file(ifn):
	# 提取出剧集序号相同的字幕文件
	epid = extract_episode_id(ifn)
	subtitle_files = []

	for x in ifn.parent.iterdir():
		if is_subtitle_file(x):
			x_epid = extract_episode_id(x)
			if epid == x_epid:
				subtitle_files.append(x)

	# 按照字幕文件与视频文件名的相似度排序
	def sk(x):
		return difflib.SequenceMatcher(None, str(ifn), str(x)).ratio()

	subtitle_files = sorted(subtitle_files, key=sk, reverse=True)
	return subtitle_files[0] if subtitle_files else None

def extract_episode_id(ifn):
	normalized_name = str(ifn)
	patterns = [
		r'S\d+E(\d+)',
		r'EP?(\d+)',
		r'CD(\d+)',
		r'DVD(\d+)', 
		r'Disk(\d+)',
		r'第.*?(\d+).*?集',
		r'Part.*?([IVX]+)',
		r'([IVX]+)',
		r'(\d+).*?of.*?\d+',
	]
	for pat in patterns:
		m = re.search(pat, normalized_name, re.I)
		if m:
			epid = m.group(1)
			return epid.upper()

if __name__ == "__main__":
	main()

