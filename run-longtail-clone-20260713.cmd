@echo off
set GIT_CONFIG_GLOBAL=C:\craton\CratonVM\git-sandbox-safe-directory.config
git clone --no-hardlinks --branch dev C:\craton\CratonVM\.git C:\craton\CratonVM\_hib_longtail_isolated_5_20260713 > C:\craton\CratonVM\hib-longtail-clone5.stdout.log 2> C:\craton\CratonVM\hib-longtail-clone5.stderr.log
