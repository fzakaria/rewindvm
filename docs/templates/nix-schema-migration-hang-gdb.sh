cat > /tmp/g.cmd <<'END'
set pagination off
handle SIGSTOP nostop noprint nopass
set unwind-on-signal on
break nix::handleSQLiteBusy
continue
bt
delete
break sqlite3_exec
continue
set $db = $rdi
printf "statement: %.200s\n", (char *) $rsi
finish
printf "sqlite3_exec returned %d\n", (int) $rax
printf "extended error code %d\n", (int) sqlite3_extended_errcode($db)
printf "errmsg %s\n", (char *) sqlite3_errmsg($db)
printf "autocommit %d\n", (int) sqlite3_get_autocommit($db)
detach
END
gdb -p @PID@ -batch -x /tmp/g.cmd 2>&1 | grep -vE "^warning|auto-load|add-auto-load|line to your|To (enable|completely)|set auto-load|For more information|info .\(gdb\)|^$"
exit
