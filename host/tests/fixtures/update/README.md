Фикстуры проверки подписи манифеста обновления (D50b, host/src/update_verify.rs).

`test-key.pem` -- ключ ТОЛЬКО ДЛЯ ТЕСТОВ, к боевому ключу релизов отношения не имеет.
Манифест и подпись сделаны настоящим `infra/release-manifest.sh 1.2.3 test-key.pem . <заглушки>`.

Перегенерация (время выпуска фиксируется переменной, иначе в манифесте будет текущее):
`RELEASE_MANIFEST_ISSUED_AT=1700000000 bash infra/release-manifest.sh 1.2.3 host/tests/fixtures/update/test-key.pem host/tests/fixtures/update host/tests/fixtures/update/rcdesk-setup.exe host/tests/fixtures/update/rcdesk-macos.pkg`
(из корня репозитория). Тесты ждут `issued_at` = 1700000000.
