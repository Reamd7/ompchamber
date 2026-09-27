/**
 * （中文套件说明）语言检测测试：各语言的整句识别、乌克兰语/俄语短语的
 * 字母级区分、无字母回退英语、引号外文词不翻转整段语言，
 * 以及 voice 挑选的优先级与 locale 解析。
 */
import { describe, expect, it } from 'vitest';
import { detectTextLanguage, languageOfLocale, pickVoiceForLanguage } from './language-detect.js';

// 文本语言检测：文字系统、特征字母与停用词计分的综合判定。
describe('detectTextLanguage', () => {
  it.each([
    ['en', 'The build is green and the tests pass, so you can merge this now.'],
    ['uk', 'Привіт! Це тестове повідомлення, і воно написане українською мовою.'],
    ['ru', 'Привет! Это тестовое сообщение, и оно написано на русском языке.'],
    ['de', 'Die Änderung ist fertig und die Tests laufen ohne Fehler durch.'],
    ['fr', 'La modification est prête et les tests passent sans erreur.'],
    ['es', 'El cambio está listo y las pruebas pasan sin errores.'],
    ['it', 'La modifica è pronta e i test passano senza errori.'],
    ['pt', 'A alteração está pronta e os testes passam sem erros, você pode continuar.'],
    ['pl', 'Zmiana jest gotowa i testy przechodzą bez błędów.'],
    ['nl', 'De wijziging is klaar en de tests slagen zonder fouten.'],
    ['cs', 'Změna je hotová a testy procházejí bez chyb.'],
    ['tr', 'Değişiklik hazır ve testler hatasız geçiyor.'],
    ['sv', 'Ändringen är klar och testerna går igenom utan fel.'],
    ['zh', '修改已经完成，所有测试都通过了。'],
    ['ja', '変更が完了し、すべてのテストに合格しました。'],
    ['ko', '변경이 완료되었고 모든 테스트를 통과했습니다.'],
  ])('detects %s', (language, text) => {
    expect(detectTextLanguage(text).language).toBe(language);
  });

  it.each([
    ['uk', 'Готово. Запушено.'],
    ['uk', 'Все ок'],
    ['uk', 'Добре, давай так зробимо'],
    ['ru', 'Хорошо, давай так и сделаем'],
    ['ru', 'Готово, всё запушено.'],
  ])('tells short %s phrases apart by letters', (language, text) => {
    expect(detectTextLanguage(text).language).toBe(language);
  });

  it('falls back to English for text without letters', () => {
    expect(detectTextLanguage('1234 ... !!!').language).toBe('en');
    expect(detectTextLanguage('').language).toBe('en');
  });

  it('does not let a single quoted foreign word flip an English paragraph', () => {
    const text = 'The façade of the building is the part that you see from the street, and it is not the same as the interior.';
    expect(detectTextLanguage(text).language).toBe('en');
  });
});

// 按语言挑选 say voice：Enhanced 变体与主 locale 优先。
describe('pickVoiceForLanguage', () => {
  // 共用的 say 风格 voice 样例列表。
  const voices = [
    { name: 'Samantha', locale: 'en_US' },
    { name: 'Daniel', locale: 'en_GB' },
    { name: 'Lesya', locale: 'uk_UA' },
    { name: 'Lesya (Enhanced)', locale: 'uk_UA' },
    { name: 'Milena', locale: 'ru_RU' },
    { name: 'Anna', locale: 'de_DE' },
  ];

  it('prefers the enhanced variant of a matching voice', () => {
    expect(pickVoiceForLanguage('uk', voices)).toBe('Lesya (Enhanced)');
  });

  it('prefers the primary locale of a language', () => {
    expect(pickVoiceForLanguage('en', voices)).toBe('Samantha');
  });

  it('returns null when no voice speaks the language', () => {
    expect(pickVoiceForLanguage('ja', voices)).toBeNull();
  });
});

// locale -> 语言子 tag 的解析。
describe('languageOfLocale', () => {
  it('reads the language subtag', () => {
    expect(languageOfLocale('uk_UA')).toBe('uk');
    expect(languageOfLocale('en-GB')).toBe('en');
    expect(languageOfLocale(null)).toBeNull();
  });
});
