@echo off
cd /d "%~dp0"
python lol_death_replay.py --before 15 --after 2
pause
