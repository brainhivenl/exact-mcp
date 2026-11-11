(() => {
  const names = [
    'EntryID', 'Key', 'OffsetID', '_Division_', 'HeaderAmountFC', 'GLAccountHeader',
    'Type', 'Journal', 'AccountHeader', 'PaymentInTransitAccount', 'SettingAmount',
    'grd_r0_K', 'grd_r0$GLAccount', 'grd_r0$GLAccount_alt', 'grd_r0$GLAccountType',
    'grd_r0$GLAccountMatching', 'grd_r0$GLAccountUseCostcenter', 'grd_r0$GLAccountUseCostunit',
    'grd_r0$GLAccountIsDefault', 'grd_r0$GLAccountIsGLBatchBooking',
    'grd_r0$GLAccountDefaultVatCodeType', 'grd_r0$GLAccountDefaultVatTransactionType',
    'grd_r0$Description', 'grd_r0$Notes', 'grd_r0$Costcenter', 'grd_r0$Costcenter_alt',
    'grd_r0$Account', 'grd_r0$Account_alt', 'grd_r0$AccountCashAllocation',
    'grd_r0$VATCode', 'grd_r0$VATCode_alt', 'grd_r0$VATPercentage', 'grd_r0$AmountFCDisplay', 'grd_r0$AmountVATFC',
    'grd_r0$InvoiceNumber', 'grd_r0$AmountFC', 'grd_r0$AmountIncludingVATFC',
    'grd_r0$EntryDate', 'grd_r0$ar', 'grd_r0$ap', 'grd_r0$PaymentTermMatchID',
    'grd_r0$GLMatchID', 'grd_r0$EnabledAsset', 'grd_r0$AmountIn', 'grd_r0$AmountOut',
    'grd_r0$RateFC', 'grd_r0$RateFC_hidden', 'grd_r0$RateFC$Currencies', 'grd_r0$PaymentInformationID',
    'grd_r0$SuggestedCountry', 'grd_r0$DirectDebitBatchTransactionsCount',
    'ActionMode', 'ActionPaymentTerms', 'CleanUpGLTransactionLinks', 'IsMarkedAsCompleted'
  ];
  for (const input of document.querySelectorAll('[name^="grd_r0$"]')) {
    if (!names.includes(input.name)) throw new Error('Unrecognized booking field; adapter review required');
  }
  const fields = {};
  for (const name of names) {
    const inputs = document.querySelectorAll(`[name="${name}"]`);
    if (inputs.length > 1) throw new Error('Ambiguous editor field');
    if (inputs.length === 1) fields[name] = inputs[0].value;
  }
  const keys = Array.from(document.querySelectorAll('input[name^="grd_r"][name$="_K"]'))
    .filter(input => /^grd_r\d+_K$/.test(input.name))
    .map(input => input.value.replace(/[{}]/g, '').toLowerCase());
  const visible = element => element.getClientRects().length > 0 && getComputedStyle(element).visibility !== 'hidden';
  const saveButtons = Array.from(document.querySelectorAll('button,input[type="button"],input[type="submit"]'))
    .filter(element => visible(element) && !element.disabled &&
      (element.getAttribute('aria-label') || element.value || element.textContent).trim() === 'Opslaan');
  if (location.pathname !== '/docs/CflEntry.aspx') throw new Error('Unexpected editor route');
  return {
    origin: location.origin, path: location.pathname,
    division: Number(fields['_Division_']), row_keys: keys, save_buttons: saveButtons.length, fields
  };
})()
