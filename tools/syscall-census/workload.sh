echo hello > a.txt; for i in 3 1 2; do echo line$i >> a.txt; done
cat a.txt | sort | uniq | wc -l
ls -la . > /dev/null; grep -c line a.txt; sed -n 2p a.txt; awk '{n++} END {print n}' a.txt
mkdir -p d/e; mv a.txt d/e/b.txt; find . -name '*.txt'; rm -rf d
x=$(date +%s); test $x -gt 0 && echo dated; sleep 0.1; true &
wait; trap 'echo trapped' INT; kill -INT $$; echo end
